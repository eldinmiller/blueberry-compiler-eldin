mod cli;

use ariadne::{Color, Label, Report, ReportKind, Source, sources};
use blueberry_codegen_core::{CodegenError, GeneratedFile};
use blueberry_generator_c as generator_c;
use blueberry_generator_cpp as generator_cpp;
use blueberry_generator_idl::generate_idl;
use blueberry_generator_python as generator_python;
use blueberry_generator_rust::generate_rust;
use blueberry_generator_typescript as generator_typescript;
use blueberry_parser::{
    Annotation, AnnotationParam, ConstValue, Definition, ImportScope, IntegerBase, IntegerLiteral,
    ParseError, parse_idl,
};
use cli::OutputFormat;
use std::collections::{HashMap, HashSet};

fn byte_offset_to_line(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

fn github_diagnostic(level: &str, file: &str, source: &str, offset: usize, message: &str) {
    let line = byte_offset_to_line(source, offset);
    eprintln!("::{level} file={file},line={line}::{message}");
}

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process,
};

fn main() {
    cli::init();

    if let Err(err) = run() {
        if err.downcast_ref::<DiagnosticAlreadyPrinted>().is_none() {
            eprintln!("error: {err}");
        }
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let options = cli::get();
    let is_dir = options.input.is_dir();

    if is_dir
        && (options.emit_rust
            || options.emit_c
            || options.emit_cpp
            || options.emit_python
            || options.emit_typescript)
    {
        return Err("directory input only supports `--emit-idl`".into());
    }

    let definitions = if is_dir {
        load_definitions_from_dir(&options.input)?
    } else {
        let source = fs::read_to_string(&options.input)?;
        let mut defs = parse_idl(&source).map_err(|err| {
            report_parse_error(&options.input, &source, &err);
            Box::new(DiagnosticAlreadyPrinted) as Box<dyn Error>
        })?;
        let file_ref: Vec<(&Path, &str, &[Definition])> =
            vec![(&options.input, source.as_str(), &defs)];
        validate_keys(&file_ref)?;
        let mut module_paths = HashSet::new();
        collect_module_paths(&defs, &mut vec![], &mut module_paths);
        validate_imports(&file_ref, &module_paths)?;
        warn_missing_message_keys(&file_ref);
        auto_assign_message_keys(&mut defs);
        defs
    };

    eprintln!(
        "Parsed {} top-level definitions from {}",
        definitions.len(),
        options.input.display()
    );

    if options.emit_idl {
        let generated = generate_idl(&definitions);
        print!("{generated}");
    }

    let output_root = options
        .output_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    if options.emit_rust {
        let files = generate_rust(&definitions).map_err(|err| fmt_codegen_error("Rust", err))?;
        write_generated_files("Rust", &output_root, &files)?;
        run_rustfmt(&output_root, &files);
    }

    if options.emit_c {
        let files =
            generator_c::generate(&definitions).map_err(|err| fmt_codegen_error("C", err))?;
        write_generated_files("C", &output_root, &files)?;
    }

    if options.emit_cpp {
        let files =
            generator_cpp::generate(&definitions).map_err(|err| fmt_codegen_error("C++", err))?;
        write_generated_files("C++", &output_root, &files)?;
    }

    if options.emit_python {
        let files = generator_python::generate(&definitions)
            .map_err(|err| fmt_codegen_error("Python", err))?;
        write_generated_files("Python", &output_root, &files)?;
    }

    if options.emit_typescript {
        let files = generator_typescript::generate(&definitions)
            .map_err(|err| fmt_codegen_error("TypeScript", err))?;
        write_generated_files("TypeScript", &output_root, &files)?;
    }

    Ok(())
}

fn report_parse_error(path: &Path, source: &str, error: &ParseError) {
    let filename = path.display().to_string();
    let f = filename.as_str();

    if cli::get().output_format == OutputFormat::Github {
        let (offset, msg) = match error {
            ParseError::UnrecognizedToken { span, token, .. } => {
                let detail = error
                    .format_expected()
                    .unwrap_or_else(|| "unexpected token".into());
                (span.0, format!("unexpected token `{token}`: {detail}"))
            }
            ParseError::UnrecognizedEof { location, .. } => {
                let detail = error
                    .format_expected()
                    .unwrap_or_else(|| "unexpected end of input".into());
                (
                    (*location).min(source.len()),
                    format!("unexpected end of input: {detail}"),
                )
            }
            ParseError::InvalidToken { location } => (*location, "invalid token".to_string()),
            ParseError::ExtraToken { span, token } => {
                (span.0, format!("unexpected extra token `{token}`"))
            }
            ParseError::Validation { message } => (0, message.clone()),
        };
        github_diagnostic("error", f, source, offset, &msg);
        return;
    }

    match error {
        ParseError::UnrecognizedToken {
            span,
            token,
            expected: _,
        } => {
            let label_msg = error
                .format_expected()
                .unwrap_or_else(|| "unexpected token".into());

            Report::build(ReportKind::Error, f, span.0)
                .with_message(format!("unexpected token `{token}`"))
                .with_label(
                    Label::new((f, span.0..span.1))
                        .with_message(label_msg)
                        .with_color(Color::Red),
                )
                .finish()
                .eprint((f, Source::from(source)))
                .unwrap();
        }
        ParseError::UnrecognizedEof {
            location,
            expected: _,
        } => {
            let loc = (*location).min(source.len());
            let label_msg = error
                .format_expected()
                .unwrap_or_else(|| "unexpected end of input".into());

            Report::build(ReportKind::Error, f, loc)
                .with_message("unexpected end of input")
                .with_label(
                    Label::new((f, loc..loc))
                        .with_message(label_msg)
                        .with_color(Color::Red),
                )
                .finish()
                .eprint((f, Source::from(source)))
                .unwrap();
        }
        ParseError::InvalidToken { location } => {
            let end = (*location + 1).min(source.len());

            Report::build(ReportKind::Error, f, *location)
                .with_message("invalid token")
                .with_label(
                    Label::new((f, *location..end))
                        .with_message("unexpected character")
                        .with_color(Color::Red),
                )
                .finish()
                .eprint((f, Source::from(source)))
                .unwrap();
        }
        ParseError::ExtraToken { span, token } => {
            Report::build(ReportKind::Error, f, span.0)
                .with_message(format!("unexpected extra token `{token}`"))
                .with_label(
                    Label::new((f, span.0..span.1))
                        .with_message("not expected here")
                        .with_color(Color::Red),
                )
                .finish()
                .eprint((f, Source::from(source)))
                .unwrap();
        }
        ParseError::Validation { message } => {
            Report::<(&str, std::ops::Range<usize>)>::build(ReportKind::Error, f, 0)
                .with_message(message)
                .finish()
                .eprint((f, Source::from(source)))
                .unwrap();
        }
    }
}

/// Sentinel error: the diagnostic was already printed via ariadne, so
/// `main()` should exit without printing anything else.
#[derive(Debug)]
struct DiagnosticAlreadyPrinted;

impl fmt::Display for DiagnosticAlreadyPrinted {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

impl Error for DiagnosticAlreadyPrinted {}

fn load_definitions_from_dir(root: &Path) -> Result<Vec<Definition>, Box<dyn Error>> {
    let idl_files = collect_idl_files(root)?;
    if idl_files.is_empty() {
        return Err(format!("no .idl files found in {}", root.display()).into());
    }

    let mut files: Vec<(PathBuf, String, Vec<Definition>)> = Vec::new();
    for path in &idl_files {
        let source = fs::read_to_string(path)?;
        let defs = parse_idl(&source).map_err(|err| {
            report_parse_error(path, &source, &err);
            Box::new(DiagnosticAlreadyPrinted) as Box<dyn Error>
        })?;
        files.push((path.clone(), source, defs));
    }

    let refs: Vec<(&Path, &str, &[Definition])> = files
        .iter()
        .map(|(p, s, d)| (p.as_path(), s.as_str(), d.as_slice()))
        .collect();
    validate_keys(&refs)?;

    let mut module_paths = HashSet::new();
    for (_, _, defs) in files.iter() {
        collect_module_paths(defs, &mut vec![], &mut module_paths);
    }
    validate_imports(&refs, &module_paths)?;
    warn_missing_message_keys(&refs);

    let mut all_defs: Vec<Definition> = Vec::new();
    for (_, _, defs) in files {
        merge_definitions(&mut all_defs, defs);
    }

    propagate_module_key_annotations(&mut all_defs);
    auto_assign_message_keys(&mut all_defs);

    Ok(all_defs)
}

fn collect_idl_files(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    collect_idl_files_recursive(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_idl_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "test") {
                continue;
            }
            collect_idl_files_recursive(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "idl") {
            out.push(path);
        }
    }
    Ok(())
}

fn is_module_key_annotation(ann: &Annotation) -> bool {
    ann.name
        .last()
        .map(|s| s.eq_ignore_ascii_case("module_key"))
        .unwrap_or(false)
}

fn propagate_module_key_annotations(definitions: &mut [Definition]) {
    let mut module_keys: HashMap<String, Annotation> = HashMap::new();
    collect_module_key_annotations(definitions, &mut module_keys);
    if !module_keys.is_empty() {
        apply_module_key_annotations(definitions, &module_keys);
    }
}

fn collect_module_key_annotations(
    definitions: &[Definition],
    keys: &mut HashMap<String, Annotation>,
) {
    for def in definitions {
        if let Definition::ModuleDef(module) = def {
            if let Some(ann) = module
                .annotations
                .iter()
                .find(|a| is_module_key_annotation(a))
            {
                keys.entry(module.node.name.clone())
                    .or_insert_with(|| ann.clone());
            }
            collect_module_key_annotations(&module.node.definitions, keys);
        }
    }
}

fn apply_module_key_annotations(
    definitions: &mut [Definition],
    keys: &HashMap<String, Annotation>,
) {
    for def in definitions.iter_mut() {
        if let Definition::ModuleDef(module) = def {
            let has_mk = module.annotations.iter().any(is_module_key_annotation);
            if !has_mk && let Some(ann) = keys.get(&module.node.name) {
                module.annotations.push(ann.clone());
            }
            apply_module_key_annotations(&mut module.node.definitions, keys);
        }
    }
}

fn is_message_key_annotation(ann: &Annotation) -> bool {
    ann.name
        .last()
        .map(|s| s.eq_ignore_ascii_case("message_key"))
        .unwrap_or(false)
}

fn annotation_integer_value(ann: &Annotation) -> Option<i128> {
    let param = ann.params.first()?;
    let value = match param {
        AnnotationParam::Named { value, .. } => value,
        AnnotationParam::Positional(v) => v,
    };
    match value {
        ConstValue::Integer(lit) => Some(lit.value),
        _ => None,
    }
}

fn find_nth_pattern(source: &str, pattern: &str, n: usize) -> (usize, usize) {
    let mut count = 0;
    let mut search_from = 0;
    while let Some(rel) = source[search_from..].find(pattern) {
        let start = search_from + rel;
        if count == n {
            let end = source[start..]
                .find(')')
                .map(|p| start + p + 1)
                .unwrap_or(start + pattern.len());
            return (start, end);
        }
        count += 1;
        search_from = start + pattern.len();
    }
    (0, pattern.len().min(source.len()))
}

struct KeyEntry {
    value: i128,
    label: String,
    file_idx: usize,
    occurrence: usize,
}

fn collect_keys(
    defs: &[Definition],
    module_path: &mut Vec<String>,
    file_idx: usize,
    mk_occurrence: &mut usize,
    msg_occurrence: &mut usize,
    module_keys: &mut Vec<(Vec<String>, KeyEntry)>,
    message_keys: &mut Vec<(String, KeyEntry)>,
) {
    for def in defs {
        match def {
            Definition::ModuleDef(module) => {
                module_path.push(module.node.name.clone());
                if let Some(ann) = module
                    .annotations
                    .iter()
                    .find(|a| is_module_key_annotation(a))
                    && let Some(value) = annotation_integer_value(ann)
                {
                    module_keys.push((
                        module_path.clone(),
                        KeyEntry {
                            value,
                            label: module.node.name.clone(),
                            file_idx,
                            occurrence: *mk_occurrence,
                        },
                    ));
                    *mk_occurrence += 1;
                }
                collect_keys(
                    &module.node.definitions,
                    module_path,
                    file_idx,
                    mk_occurrence,
                    msg_occurrence,
                    module_keys,
                    message_keys,
                );
                module_path.pop();
            }
            Definition::MessageDef(msg) => {
                if let Some(ann) = msg
                    .annotations
                    .iter()
                    .find(|a| is_message_key_annotation(a))
                    && let Some(value) = annotation_integer_value(ann)
                {
                    let root = module_path.first().cloned().unwrap_or_default();
                    message_keys.push((
                        root,
                        KeyEntry {
                            value,
                            label: msg.node.name.clone(),
                            file_idx,
                            occurrence: *msg_occurrence,
                        },
                    ));
                    *msg_occurrence += 1;
                }
            }
            _ => {}
        }
    }
}

fn validate_keys(files: &[(&Path, &str, &[Definition])]) -> Result<(), Box<dyn Error>> {
    let mut module_keys: Vec<(Vec<String>, KeyEntry)> = Vec::new();
    let mut message_keys: Vec<(String, KeyEntry)> = Vec::new();

    for (file_idx, (_, _, defs)) in files.iter().enumerate() {
        let mut path = Vec::new();
        let mut mk_occ = 0;
        let mut msg_occ = 0;
        collect_keys(
            defs,
            &mut path,
            file_idx,
            &mut mk_occ,
            &mut msg_occ,
            &mut module_keys,
            &mut message_keys,
        );
    }

    let mut had_errors = false;

    let mut mk_map: HashMap<Vec<String>, usize> = HashMap::new();
    for (idx, (path, entry)) in module_keys.iter().enumerate() {
        if let Some(&existing_idx) = mk_map.get(path) {
            let existing = &module_keys[existing_idx].1;
            if existing.value != entry.value {
                report_conflicting_module_key(path, existing, entry, files);
                had_errors = true;
            }
        } else {
            mk_map.insert(path.clone(), idx);
        }
    }

    let mut msg_map: HashMap<(String, i128), usize> = HashMap::new();
    for (idx, (root, entry)) in message_keys.iter().enumerate() {
        let key = (root.clone(), entry.value);
        if let Some(&existing_idx) = msg_map.get(&key) {
            let existing = &message_keys[existing_idx].1;
            if existing.label != entry.label {
                report_duplicate_message_key(root, existing, entry, files);
                had_errors = true;
            }
        } else {
            msg_map.insert(key, idx);
        }
    }

    if had_errors {
        Err(Box::new(DiagnosticAlreadyPrinted))
    } else {
        Ok(())
    }
}

fn report_conflicting_module_key(
    module_path: &[String],
    first: &KeyEntry,
    second: &KeyEntry,
    files: &[(&Path, &str, &[Definition])],
) {
    let (first_path, first_source, _) = files[first.file_idx];
    let (second_path, second_source, _) = files[second.file_idx];

    let first_name = first_path.display().to_string();
    let second_name = second_path.display().to_string();

    let (start1, end1) = find_nth_pattern(first_source, "@module_key", first.occurrence);
    let (start2, end2) = find_nth_pattern(second_source, "@module_key", second.occurrence);

    if cli::get().output_format == OutputFormat::Github {
        let mod_path = module_path.join("::");
        github_diagnostic(
            "error",
            &second_name,
            second_source,
            start2,
            &format!(
                "conflicting @module_key values for module `{}`: \
                 0x{:X} here vs 0x{:X} in {}:{}",
                mod_path,
                second.value,
                first.value,
                first_name,
                byte_offset_to_line(first_source, start1),
            ),
        );
        return;
    }

    Report::build(ReportKind::Error, second_name.clone(), start2)
        .with_message(format!(
            "conflicting @module_key values for module `{}`",
            module_path.join("::")
        ))
        .with_label(
            Label::new((second_name.clone(), start2..end2))
                .with_message(format!("defined as @module_key(0x{:X})", second.value))
                .with_color(Color::Red),
        )
        .with_label(
            Label::new((first_name.clone(), start1..end1))
                .with_message(format!(
                    "previously defined as @module_key(0x{:X})",
                    first.value
                ))
                .with_color(Color::Yellow),
        )
        .finish()
        .eprint(sources(vec![
            (first_name, first_source),
            (second_name, second_source),
        ]))
        .unwrap();
}

fn report_duplicate_message_key(
    root_module: &str,
    first: &KeyEntry,
    second: &KeyEntry,
    files: &[(&Path, &str, &[Definition])],
) {
    let (first_path, first_source, _) = files[first.file_idx];
    let (second_path, second_source, _) = files[second.file_idx];

    let first_name = first_path.display().to_string();
    let second_name = second_path.display().to_string();

    let (start1, end1) = find_nth_pattern(first_source, "@message_key", first.occurrence);
    let (start2, end2) = find_nth_pattern(second_source, "@message_key", second.occurrence);

    if cli::get().output_format == OutputFormat::Github {
        github_diagnostic(
            "error",
            &second_name,
            second_source,
            start2,
            &format!(
                "duplicate @message_key(0x{:X}) in module `{}`: \
                 used by `{}` here and `{}` in {}:{}",
                second.value,
                root_module,
                second.label,
                first.label,
                first_name,
                byte_offset_to_line(first_source, start1),
            ),
        );
        return;
    }

    Report::build(ReportKind::Error, second_name.clone(), start2)
        .with_message(format!(
            "duplicate @message_key(0x{:X}) in module `{}`",
            second.value, root_module
        ))
        .with_label(
            Label::new((second_name.clone(), start2..end2))
                .with_message(format!("used by message `{}`", second.label))
                .with_color(Color::Red),
        )
        .with_label(
            Label::new((first_name.clone(), start1..end1))
                .with_message(format!("first used by message `{}`", first.label))
                .with_color(Color::Yellow),
        )
        .finish()
        .eprint(sources(vec![
            (first_name, first_source),
            (second_name, second_source),
        ]))
        .unwrap();
}

fn collect_module_paths(
    defs: &[Definition],
    current_path: &mut Vec<String>,
    paths: &mut HashSet<Vec<String>>,
) {
    for def in defs {
        if let Definition::ModuleDef(module) = def {
            current_path.push(module.node.name.clone());
            paths.insert(current_path.clone());
            collect_module_paths(&module.node.definitions, current_path, paths);
            current_path.pop();
        }
    }
}

fn validate_imports(
    files: &[(&Path, &str, &[Definition])],
    module_paths: &HashSet<Vec<String>>,
) -> Result<(), Box<dyn Error>> {
    let mut had_errors = false;
    for &(path, source, defs) in files {
        check_imports_recursive(defs, path, source, module_paths, &mut had_errors);
    }

    if had_errors {
        Err(Box::new(DiagnosticAlreadyPrinted))
    } else {
        Ok(())
    }
}

fn check_imports_recursive(
    defs: &[Definition],
    file_path: &Path,
    source: &str,
    module_paths: &HashSet<Vec<String>>,
    had_errors: &mut bool,
) {
    for def in defs {
        match def {
            Definition::ImportDef(import) => {
                if let ImportScope::Scoped(ref segments) = import.node.scope
                    && !module_paths.contains(segments)
                {
                    report_unresolved_import(segments, file_path, source);
                    *had_errors = true;
                }
            }
            Definition::ModuleDef(module) => {
                check_imports_recursive(
                    &module.node.definitions,
                    file_path,
                    source,
                    module_paths,
                    had_errors,
                );
            }
            _ => {}
        }
    }
}

fn report_unresolved_import(segments: &[String], file_path: &Path, source: &str) {
    let filename = file_path.display().to_string();
    let import_text = segments.join("::");
    let search = format!("import {}", import_text);

    let start = source
        .find(&search)
        .or_else(|| source.find(&format!("import ::{}", import_text)))
        .unwrap_or(0);
    let end = source[start..]
        .find(';')
        .map(|p| start + p + 1)
        .unwrap_or(start + search.len());

    if cli::get().output_format == OutputFormat::Github {
        github_diagnostic(
            "error",
            &filename,
            source,
            start,
            &format!(
                "unresolved import `{}`: no module `{}` exists",
                import_text, import_text
            ),
        );
        return;
    }

    Report::build(ReportKind::Error, filename.as_str(), start)
        .with_message(format!("unresolved import `{}`", import_text))
        .with_label(
            Label::new((filename.as_str(), start..end))
                .with_message(format!("no module `{}` exists", import_text))
                .with_color(Color::Red),
        )
        .finish()
        .eprint((filename.as_str(), Source::from(source)))
        .unwrap();
}

fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn message_absolute_path(module_path: &[String], message_name: &str) -> String {
    let mut path = String::from("::");
    for seg in module_path {
        path.push_str(&seg.to_lowercase());
        path.push_str("::");
    }
    path.push_str(&message_name.to_lowercase());
    path
}

fn warn_missing_message_keys(files: &[(&Path, &str, &[Definition])]) {
    for &(path, source, defs) in files {
        warn_missing_keys_recursive(defs, &mut vec![], path, source);
    }
}

fn warn_missing_keys_recursive(
    defs: &[Definition],
    module_path: &mut Vec<String>,
    file_path: &Path,
    source: &str,
) {
    for def in defs {
        match def {
            Definition::MessageDef(msg) => {
                let has_mk = msg.annotations.iter().any(is_message_key_annotation);
                if !has_mk {
                    let abs_path = message_absolute_path(module_path, &msg.node.name);
                    let key = crc16_ccitt(abs_path.as_bytes());
                    report_missing_message_key(&msg.node.name, &abs_path, key, file_path, source);
                }
            }
            Definition::ModuleDef(module) => {
                module_path.push(module.node.name.clone());
                warn_missing_keys_recursive(
                    &module.node.definitions,
                    module_path,
                    file_path,
                    source,
                );
                module_path.pop();
            }
            _ => {}
        }
    }
}

fn report_missing_message_key(
    message_name: &str,
    abs_path: &str,
    auto_key: u16,
    file_path: &Path,
    source: &str,
) {
    let filename = file_path.display().to_string();
    let search = format!("message {}", message_name);
    let start = source.find(&search).unwrap_or(0);
    let end = start + search.len();

    if cli::get().output_format == OutputFormat::Github {
        github_diagnostic(
            "warning",
            &filename,
            source,
            start,
            &format!(
                "message `{}` has no @message_key — auto-assigned @message_key(0x{:X}) \
                 from CRC-16 CCITT of `{}`",
                message_name, auto_key, abs_path,
            ),
        );
        return;
    }

    Report::build(ReportKind::Warning, filename.as_str(), start)
        .with_message(format!("message `{}` has no @message_key", message_name))
        .with_label(
            Label::new((filename.as_str(), start..end))
                .with_message("missing @message_key annotation")
                .with_color(Color::Yellow),
        )
        .with_note(format!(
            "@message_key(0x{:X}) will be auto-assigned from CRC-16 of `{}`",
            auto_key, abs_path
        ))
        .with_help(
            "the hash uses CRC-16 CCITT of the lowercased absolute path; \
             collisions are resolved by incrementing it by one",
        )
        .finish()
        .eprint((filename.as_str(), Source::from(source)))
        .unwrap();
}

fn auto_assign_message_keys(definitions: &mut [Definition]) {
    let mut used_keys: HashSet<u16> = HashSet::new();
    collect_existing_message_keys(definitions, &mut used_keys);
    assign_missing_message_keys(definitions, &mut vec![], &mut used_keys);
}

fn collect_existing_message_keys(definitions: &[Definition], used: &mut HashSet<u16>) {
    for def in definitions {
        match def {
            Definition::MessageDef(msg) => {
                if let Some(ann) = msg
                    .annotations
                    .iter()
                    .find(|a| is_message_key_annotation(a))
                    && let Some(value) = annotation_integer_value(ann)
                    && (0..=u16::MAX as i128).contains(&value)
                {
                    used.insert(value as u16);
                }
            }
            Definition::ModuleDef(module) => {
                collect_existing_message_keys(&module.node.definitions, used);
            }
            _ => {}
        }
    }
}

fn assign_missing_message_keys(
    definitions: &mut [Definition],
    module_path: &mut Vec<String>,
    used_keys: &mut HashSet<u16>,
) {
    for def in definitions.iter_mut() {
        match def {
            Definition::MessageDef(msg) => {
                let has_mk = msg.annotations.iter().any(is_message_key_annotation);
                if !has_mk {
                    let abs_path = message_absolute_path(module_path, &msg.node.name);
                    let mut key = crc16_ccitt(abs_path.as_bytes());
                    while used_keys.contains(&key) {
                        key = key.wrapping_add(1);
                    }
                    used_keys.insert(key);

                    msg.annotations.push(Annotation {
                        name: vec!["message_key".to_string()],
                        params: vec![AnnotationParam::Positional(ConstValue::Integer(
                            IntegerLiteral::new(key as i128, IntegerBase::Hexadecimal),
                        ))],
                    });
                }
            }
            Definition::ModuleDef(module) => {
                module_path.push(module.node.name.clone());
                assign_missing_message_keys(&mut module.node.definitions, module_path, used_keys);
                module_path.pop();
            }
            _ => {}
        }
    }
}

/// Merges `incoming` definitions into `target`, combining `ModuleDef` nodes
/// that share the same name at the same level instead of duplicating them.
fn merge_definitions(target: &mut Vec<Definition>, incoming: Vec<Definition>) {
    for def in incoming {
        if let Definition::ModuleDef(ref incoming_mod) = def {
            let existing = target.iter_mut().find(
                |d| matches!(d, Definition::ModuleDef(m) if m.node.name == incoming_mod.node.name),
            );
            if let Some(Definition::ModuleDef(existing_mod)) = existing {
                let inner = incoming_mod.node.definitions.clone();
                merge_definitions(&mut existing_mod.node.definitions, inner);
                continue;
            }
        }
        target.push(def);
    }
}

fn fmt_codegen_error(language: &str, err: CodegenError) -> Box<dyn Error> {
    let message = match err {
        CodegenError::MissingTopic { message } => {
            format!("{language} generation failed: missing @topic for message {message}")
        }
        CodegenError::UnsupportedMessageBase { message } => format!(
            "{language} generation failed: message {message} uses inheritance which is unsupported"
        ),
        CodegenError::UnsupportedMemberType {
            message,
            member,
            type_name,
        } => format!(
            "{language} generation failed: member {member} in {message} uses unsupported type {type_name}"
        ),
        CodegenError::DuplicateMessageKey {
            module_key,
            message_key,
            first,
            second,
        } => format!(
            "{language} generation failed: duplicate @message_key(0x{message_key:04x}) in module 0x{module_key:04x}: used by {first} and {second}"
        ),
    };
    message.into()
}

fn run_rustfmt(output_root: &Path, files: &[GeneratedFile]) {
    use std::io::Write;

    let rs_paths: Vec<_> = files
        .iter()
        .filter(|f| f.path.ends_with(".rs"))
        .map(|f| output_root.join(&f.path))
        .collect();

    if rs_paths.is_empty() {
        return;
    }

    let mut formatted = 0usize;
    for path in &rs_paths {
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let result = std::process::Command::new("rustfmt")
            .arg("--edition")
            .arg("2021")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(source.as_bytes())?;
                child.wait_with_output()
            });
        match result {
            Ok(out) if out.status.success() => {
                let _ = fs::write(path, &out.stdout);
                formatted += 1;
            }
            _ => {}
        }
    }
    println!(
        "Formatted {formatted}/{} Rust files with rustfmt",
        rs_paths.len()
    );
}

fn write_generated_files(
    language: &str,
    output_root: &Path,
    files: &[GeneratedFile],
) -> Result<(), Box<dyn Error>> {
    if files.is_empty() {
        println!("\n// generated {language} files: none");
        return Ok(());
    }
    println!(
        "\n// writing generated {language} files under {}",
        output_root.display()
    );
    for file in files {
        let path = output_root.join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, &file.contents)?;
        println!("Wrote {}", path.display());
    }

    Ok(())
}

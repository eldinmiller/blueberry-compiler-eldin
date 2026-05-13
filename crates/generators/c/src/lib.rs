#![allow(dead_code)]
//! C emitter that reproduces the legacy `blueberry-schema-parser` ABI used by
//! the `bluerobotics/blueberry-c` firmware bindings.
//!
//! The new compiler historically emitted a two-layer `blueberry_runtime.{h,c}`
//! + `blueberry_messages.{h,c}` shape that the firmware cannot consume directly.
//! This module instead generates a single pair `inc/blueberry_devices.h` and
//! `src/blueberry_devices.c` matching the legacy banner, naming conventions,
//! per-message `add<Message>` builders, per-field `get<Message><Field>` /
//! `is<...>Present` accessors, and `init<Message><Field>` / sequence-element
//! accessors / `*SequenceLength` helpers.
//!
//! Wire layout matches the legacy generator: an 8-byte fixed header
//! (`module+message key`, `length-in-words`, `max-ordinal`, padding), followed
//! by message fields packed by alignment with a stable look-ahead reorder, and
//! sequence placeholders stored inline with element data appended to the buffer
//! tail via the runtime helpers from `<blueberry-message.h>`.

use std::collections::BTreeMap;
use std::fmt::Write;

use blueberry_ast::{
    Annotation, AnnotationParam, Commented, ConstValue, Definition, EnumDef, EnumMember, Member,
    MessageDef, ModuleDef, StructDef, Type, TypeDef,
};
use blueberry_codegen_core::{CodegenError, GeneratedFile, map_builtin_ident};

const HEADER_PATH: &str = "inc/blueberry_devices.h";
const SOURCE_PATH: &str = "src/blueberry_devices.c";

const MESSAGE_HEADER_BYTES: u32 = 8;
const SEQUENCE_PLACEHOLDER_BYTES: u32 = 4;
const BOOL_MASK_BIT0: &str = "(1 << 0)";

/// Entry point used by the CLI to generate the legacy-shaped `blueberry-c` pair.
pub fn generate(definitions: &[Definition]) -> Result<Vec<GeneratedFile>, CodegenError> {
    let mut ctx = Context::new();
    ctx.collect(definitions, &mut Vec::new(), None)?;
    let header = render_header(&ctx);
    let source = render_source(&ctx);
    Ok(vec![
        GeneratedFile {
            path: HEADER_PATH.to_string(),
            contents: header,
        },
        GeneratedFile {
            path: SOURCE_PATH.to_string(),
            contents: source,
        },
    ])
}

// ---------------------------------------------------------------------------
// Internal AST mirror with full layout information.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct EnumModel {
    /// IDL identifier, e.g. `HwType` or `PortPinEnum`.
    name: String,
    /// C typedef identifier — `<name>` if it already ends with `Enum`, else `<name>Enum`.
    typedef: String,
    /// SCREAMING_SNAKE prefix applied to each enumerator before joining with the IDL value name.
    value_prefix: String,
    /// Underlying C primitive used by the firmware's `setBb*`/`getBb*` helpers.
    base: Type,
    /// Brief description, lifted from the IDL banner comments.
    comments: Vec<String>,
    values: Vec<EnumValueModel>,
}

#[derive(Debug, Clone)]
struct EnumValueModel {
    name: String,
    literal: String,
    comments: Vec<String>,
}

#[derive(Debug, Clone)]
struct StructModel {
    /// Layout of each sub-field after alignment-aware reordering.
    fields: Vec<StructFieldLayout>,
    /// Aligned byte size of one element (used as the sequence element byte count).
    element_size: u32,
}

#[derive(Debug, Clone)]
struct StructFieldLayout {
    /// Source field name in IDL order; first letter lower-case, used in accessor names.
    name: String,
    ty: Type,
    /// Byte offset within the struct element.
    offset: u32,
    comments: Vec<String>,
}

#[derive(Debug, Clone)]
struct MessageModel {
    /// IDL identifier (e.g. `VersionMessage`).
    name: String,
    /// Module path that owns this message, used to derive the macro prefix
    /// (`BLUEBERRY_DEVICES_` for `::Blueberry::Devices`).
    module_path: Vec<String>,
    module_key: u16,
    message_key: u16,
    topic: String,
    comments: Vec<String>,
    fields: Vec<MessageFieldModel>,
    /// Total body size including the 8-byte header and any sequence placeholders,
    /// padded to a 4-byte boundary.
    body_length: u32,
    /// Highest field ordinal (the last field's ordinal). Header fields use 0-2.
    max_ordinal: u8,
}

#[derive(Debug, Clone)]
struct MessageFieldModel {
    /// Source field name (camelCase, used to derive both the C identifier and
    /// the SCREAMING_SNAKE constants).
    name: String,
    /// Ordinal in IDL order. Header reserves 0-2.
    ordinal: u8,
    /// Byte offset within the message body.
    offset: u32,
    comments: Vec<String>,
    kind: MessageFieldKind,
}

#[derive(Debug, Clone)]
enum MessageFieldKind {
    /// Primitive, enum, or fixed-length string. The wire type is what is written
    /// into the byte at `offset`.
    Scalar(ScalarSpec),
    /// Length-prefixed sequence placeholder; the element layout follows.
    Sequence(SequenceSpec),
}

#[derive(Debug, Clone)]
struct ScalarSpec {
    /// Resolved type (with typedefs / aliases collapsed).
    ty: Type,
}

#[derive(Debug, Clone)]
struct SequenceSpec {
    /// Per-element layout — either a struct (multi sub-field) or a single primitive.
    element: SequenceElement,
}

#[derive(Debug, Clone)]
enum SequenceElement {
    Struct(StructModel),
    Primitive(Type),
}

// ---------------------------------------------------------------------------
// Resolution / type-registry / collection pass.
// ---------------------------------------------------------------------------

struct Context {
    /// Lookup of all named definitions keyed by their fully-qualified path.
    typedefs: BTreeMap<Vec<String>, (Type, Vec<String>)>,
    structs: BTreeMap<Vec<String>, (StructDef, Vec<String>)>,
    enums: BTreeMap<Vec<String>, EnumDef>,
    /// Modules already seen along with the `@module_key` resolved at their level.
    /// Used so messages inherit their containing module's key when none is set
    /// directly on the message itself.
    messages: Vec<MessageModel>,
    enum_models: Vec<EnumModel>,
}

impl Context {
    fn new() -> Self {
        Self {
            typedefs: BTreeMap::new(),
            structs: BTreeMap::new(),
            enums: BTreeMap::new(),
            messages: Vec::new(),
            enum_models: Vec::new(),
        }
    }

    fn collect(
        &mut self,
        defs: &[Definition],
        scope: &mut Vec<String>,
        parent_module_key: Option<u16>,
    ) -> Result<(), CodegenError> {
        // First, index typedefs / structs / enums so message bodies can resolve them.
        for def in defs {
            match def {
                Definition::TypeDef(t) => {
                    let mut path = scope.clone();
                    path.push(t.node.name.clone());
                    self.typedefs
                        .insert(path, (t.node.base_type.clone(), scope.clone()));
                }
                Definition::StructDef(s) => {
                    let mut path = scope.clone();
                    path.push(s.node.name.clone());
                    self.structs.insert(path, (s.node.clone(), scope.clone()));
                }
                Definition::EnumDef(e) => {
                    let mut path = scope.clone();
                    path.push(e.node.name.clone());
                    self.enums.insert(path, e.node.clone());
                }
                Definition::ModuleDef(m) => {
                    scope.push(m.node.name.clone());
                    let mk =
                        annotation_u16(&m.annotations, "module_key").or(parent_module_key);
                    self.collect(&m.node.definitions, scope, mk)?;
                    scope.pop();
                }
                _ => {}
            }
        }

        // Now record enum models and message layouts in source order, while still
        // honouring the indexed lookups built above.
        for def in defs {
            match def {
                Definition::EnumDef(e) => {
                    self.enum_models.push(self.build_enum(e));
                }
                Definition::ModuleDef(_) => {
                    // Already handled in the pass above.
                }
                Definition::MessageDef(m) => {
                    let model = self.build_message(scope, m, parent_module_key)?;
                    self.messages.push(model);
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn build_enum(&self, e: &Commented<EnumDef>) -> EnumModel {
        let base = e.node.base_type.clone().unwrap_or(Type::UnsignedLong);
        let typedef = enum_typedef_name(&e.node.name);
        let value_prefix = to_screaming_snake(&e.node.name);
        let values: Vec<EnumValueModel> = e
            .node
            .enumerators
            .iter()
            .map(|m| EnumValueModel {
                name: m.name.clone(),
                literal: enum_value_literal(m, &base),
                comments: m.comments.clone(),
            })
            .collect();
        EnumModel {
            name: e.node.name.clone(),
            typedef,
            value_prefix,
            base,
            comments: e.comments.clone(),
            values,
        }
    }

    fn build_message(
        &self,
        scope: &[String],
        m: &Commented<MessageDef>,
        parent_module_key: Option<u16>,
    ) -> Result<MessageModel, CodegenError> {
        let module_key = annotation_u16(&m.annotations, "module_key")
            .or(parent_module_key)
            .unwrap_or(0);
        let message_key =
            annotation_u16(&m.annotations, "message_key").unwrap_or(0);
        let topic = annotation_string(&m.annotations, "topic").ok_or_else(|| {
            CodegenError::MissingTopic {
                message: scoped(scope, &m.node.name),
            }
        })?;

        // Build a `(name, comments, resolved-type)` triple per IDL field, then
        // run alignment-aware placement to compute byte offsets while keeping
        // the IDL order for ordinal assignment.
        let mut declared: Vec<(String, Vec<String>, Type)> = Vec::new();
        for member in &m.node.members {
            let resolved = self.resolve_type(&member.node.type_, scope);
            declared.push((member.node.name.clone(), member.comments.clone(), resolved));
        }
        let offsets = compute_offsets(self, &declared)?;

        // Materialise each field with its IDL-order ordinal (starts at 3 after
        // the 3 implicit header fields: module-message-key, length, max-ordinal)
        // and the alignment-derived offset.
        let mut fields = Vec::new();
        for (idx, (name, comments, ty)) in declared.iter().enumerate() {
            let offset = offsets[idx];
            let ordinal = (idx + 3) as u8;
            let kind = self.build_field_kind(ty, &m.node.name, name)?;
            fields.push(MessageFieldModel {
                name: name.clone(),
                ordinal,
                offset,
                comments: comments.clone(),
                kind,
            });
        }

        let total_body = compute_body_length(self, &offsets, &declared)?;
        let max_ordinal = fields.last().map(|f| f.ordinal).unwrap_or(2);

        Ok(MessageModel {
            name: m.node.name.clone(),
            module_path: scope.to_vec(),
            module_key,
            message_key,
            topic,
            comments: m.comments.clone(),
            fields,
            body_length: total_body,
            max_ordinal,
        })
    }

    /// Resolve a `Type` by collapsing typedefs and mapping `int8`/`uint8`/...
    /// scoped-name aliases back to their primitive counterparts. Struct, enum,
    /// and message names are preserved as `Type::ScopedName(fully_qualified_path)`.
    fn resolve_type(&self, ty: &Type, scope: &[String]) -> Type {
        match ty {
            Type::Sequence { element_type, size } => Type::Sequence {
                element_type: Box::new(self.resolve_type(element_type, scope)),
                size: *size,
            },
            Type::Array {
                element_type,
                dimensions,
            } => {
                let element = self.resolve_type(element_type, scope);
                let mut resolved = element;
                for &dim in dimensions.iter().rev() {
                    resolved = Type::Sequence {
                        element_type: Box::new(resolved),
                        size: Some(dim),
                    };
                }
                resolved
            }
            Type::ScopedName(name) => {
                if let [single] = name.as_slice()
                    && let Some(mapped) = map_builtin_ident(single)
                {
                    return mapped;
                }
                if let Some(path) = self.lookup(name, scope, &self.typedefs) {
                    let (inner, inner_scope) = self.typedefs.get(&path).expect("typedef");
                    return self.resolve_type(inner, inner_scope);
                }
                if let Some(path) = self.lookup(name, scope, &self.structs) {
                    return Type::ScopedName(path);
                }
                if let Some(path) = self.lookup(name, scope, &self.enums) {
                    return Type::ScopedName(path);
                }
                Type::ScopedName(name.clone())
            }
            other => other.clone(),
        }
    }

    fn lookup<V>(
        &self,
        name: &[String],
        scope: &[String],
        table: &BTreeMap<Vec<String>, V>,
    ) -> Option<Vec<String>> {
        for prefix in (0..=scope.len()).rev() {
            let mut candidate = scope[..prefix].to_vec();
            candidate.extend_from_slice(name);
            if table.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        // Fall back to suffix matching (single-result) when the bare name
        // appears at a different scope. This mirrors the existing typescript /
        // codegen-core resolver, but only commits if the match is unambiguous.
        let matches: Vec<&Vec<String>> = table.keys().filter(|p| p.ends_with(name)).collect();
        if matches.len() == 1 {
            return Some(matches[0].clone());
        }
        None
    }

    fn build_field_kind(
        &self,
        ty: &Type,
        message_name: &str,
        field_name: &str,
    ) -> Result<MessageFieldKind, CodegenError> {
        match ty {
            Type::Sequence { element_type, .. } => {
                let element = match element_type.as_ref() {
                    Type::ScopedName(path) => {
                        if let Some((struct_def, struct_scope)) = self.structs.get(path) {
                            SequenceElement::Struct(self.build_struct(struct_def, struct_scope)?)
                        } else if self.enums.contains_key(path) {
                            // Sequences of enums fall under the same accessor pattern as
                            // primitive sequences; the wire size is the enum's base type.
                            SequenceElement::Primitive(Type::ScopedName(path.clone()))
                        } else {
                            return Err(CodegenError::UnsupportedMemberType {
                                message: message_name.to_string(),
                                member: field_name.to_string(),
                                type_name: path.join("::"),
                            });
                        }
                    }
                    other => SequenceElement::Primitive(other.clone()),
                };
                Ok(MessageFieldKind::Sequence(SequenceSpec { element }))
            }
            Type::ScopedName(path) => {
                if let Some((struct_def, struct_scope)) = self.structs.get(path) {
                    // Inline structs (i.e., struct used as a message field directly,
                    // not via a typedef sequence) get expanded but the legacy
                    // generator we are matching doesn't surface this case in the
                    // production dictionary. Treat them as an error until needed.
                    let _ = (struct_def, struct_scope);
                    return Err(CodegenError::UnsupportedMemberType {
                        message: message_name.to_string(),
                        member: field_name.to_string(),
                        type_name: path.join("::"),
                    });
                }
                Ok(MessageFieldKind::Scalar(ScalarSpec {
                    ty: Type::ScopedName(path.clone()),
                }))
            }
            Type::String { bound: Some(_) } => Ok(MessageFieldKind::Scalar(ScalarSpec {
                ty: ty.clone(),
            })),
            Type::String { bound: None } => Err(CodegenError::UnsupportedMemberType {
                message: message_name.to_string(),
                member: field_name.to_string(),
                type_name: "string (unbounded; use `string<N>` for embedded targets)".to_string(),
            }),
            _ => Ok(MessageFieldKind::Scalar(ScalarSpec { ty: ty.clone() })),
        }
    }

    fn build_struct(
        &self,
        struct_def: &StructDef,
        scope: &[String],
    ) -> Result<StructModel, CodegenError> {
        let mut declared: Vec<(String, Vec<String>, Type)> = Vec::new();
        for member in &struct_def.members {
            let resolved = self.resolve_type(&member.node.type_, scope);
            declared.push((member.node.name.clone(), member.comments.clone(), resolved));
        }
        let offsets = compute_struct_offsets(self, &declared)?;
        let element_size = compute_aligned_struct_size(self, &offsets, &declared)?;
        let mut fields = Vec::new();
        for (idx, (name, comments, ty)) in declared.iter().enumerate() {
            fields.push(StructFieldLayout {
                name: name.clone(),
                ty: ty.clone(),
                offset: offsets[idx],
                comments: comments.clone(),
            });
        }
        Ok(StructModel {
            fields,
            element_size,
        })
    }

    fn enum_base(&self, ty: &Type) -> Option<&Type> {
        match ty {
            Type::ScopedName(path) => self
                .enums
                .get(path)
                .and_then(|def| def.base_type.as_ref()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Wire size, alignment, and offset computation.
// ---------------------------------------------------------------------------

/// Returns the wire size (in bytes) of a single value of `ty`, ignoring sequence
/// element data which is appended to the buffer tail rather than stored inline.
fn wire_size(ctx: &Context, ty: &Type) -> Result<u32, CodegenError> {
    Ok(match ty {
        Type::Boolean | Type::Char | Type::Octet => 1,
        Type::Short | Type::UnsignedShort => 2,
        Type::Long | Type::UnsignedLong | Type::Float => 4,
        Type::LongLong | Type::UnsignedLongLong | Type::Double => 8,
        Type::String { bound: Some(n) } => *n,
        Type::String { bound: None } => {
            return Err(CodegenError::UnsupportedMemberType {
                message: "<unknown>".to_string(),
                member: "<unknown>".to_string(),
                type_name: "string".to_string(),
            });
        }
        Type::Sequence { .. } => SEQUENCE_PLACEHOLDER_BYTES,
        Type::ScopedName(path) => {
            if let Some(def) = ctx.enums.get(path) {
                let base = def.base_type.clone().unwrap_or(Type::UnsignedLong);
                wire_size(ctx, &base)?
            } else {
                4
            }
        }
        _ => 4,
    })
}

/// Returns the alignment requirement (in bytes) for a value of `ty`. Sequence
/// placeholders take 4-byte alignment to keep their two-uint16 headers word-aligned.
fn wire_align(ctx: &Context, ty: &Type) -> u32 {
    match ty {
        Type::Boolean | Type::Char | Type::Octet | Type::String { .. } => 1,
        Type::Short | Type::UnsignedShort => 2,
        Type::Long | Type::UnsignedLong | Type::Float => 4,
        Type::LongLong | Type::UnsignedLongLong | Type::Double => 8,
        Type::Sequence { .. } => 4,
        Type::ScopedName(path) => {
            if let Some(def) = ctx.enums.get(path) {
                let base = def.base_type.clone().unwrap_or(Type::UnsignedLong);
                wire_align(ctx, &base)
            } else {
                4
            }
        }
        _ => 4,
    }
}

/// Look-ahead aligning placer used by the legacy generator: place the next IDL
/// field if its alignment fits at the current write position; otherwise pull the
/// nearest later field that does fit forward. Returns one offset per declared
/// field, in IDL order, starting after the 8-byte message header.
fn compute_offsets(
    ctx: &Context,
    declared: &[(String, Vec<String>, Type)],
) -> Result<Vec<u32>, CodegenError> {
    place_with_alignment(ctx, declared, MESSAGE_HEADER_BYTES)
}

fn compute_struct_offsets(
    ctx: &Context,
    declared: &[(String, Vec<String>, Type)],
) -> Result<Vec<u32>, CodegenError> {
    place_with_alignment(ctx, declared, 0)
}

fn place_with_alignment(
    ctx: &Context,
    declared: &[(String, Vec<String>, Type)],
    start_offset: u32,
) -> Result<Vec<u32>, CodegenError> {
    let n = declared.len();
    let mut offsets = vec![0_u32; n];
    let mut placed = vec![false; n];
    let mut pos = start_offset;
    let mut placed_count = 0usize;

    while placed_count < n {
        // First, try the nearest IDL-order field that fits at `pos` without
        // padding.  If none fits, fall back to placing the next IDL-order field
        // (and paying the padding) so the loop always makes progress.
        let mut chosen: Option<usize> = None;
        for (i, (_, _, ty)) in declared.iter().enumerate() {
            if placed[i] {
                continue;
            }
            let align = wire_align(ctx, ty);
            if align <= 1 || pos.is_multiple_of(align) {
                chosen = Some(i);
                break;
            }
            if chosen.is_none() {
                chosen = Some(i);
            }
        }

        let i = chosen.expect("at least one unplaced field");
        let align = wire_align(ctx, &declared[i].2);
        if align > 1 {
            let rem = pos % align;
            if rem != 0 {
                pos += align - rem;
            }
        }
        offsets[i] = pos;
        pos += wire_size(ctx, &declared[i].2)?;
        placed[i] = true;
        placed_count += 1;
    }
    Ok(offsets)
}

fn compute_body_length(
    ctx: &Context,
    offsets: &[u32],
    declared: &[(String, Vec<String>, Type)],
) -> Result<u32, CodegenError> {
    let mut max_end = MESSAGE_HEADER_BYTES;
    for (i, (_, _, ty)) in declared.iter().enumerate() {
        let end = offsets[i] + wire_size(ctx, ty)?;
        if end > max_end {
            max_end = end;
        }
    }
    Ok(align_up(max_end, 4))
}

fn compute_aligned_struct_size(
    ctx: &Context,
    offsets: &[u32],
    declared: &[(String, Vec<String>, Type)],
) -> Result<u32, CodegenError> {
    let mut max_end = 0;
    let mut max_align = 1;
    for (i, (_, _, ty)) in declared.iter().enumerate() {
        let end = offsets[i] + wire_size(ctx, ty)?;
        if end > max_end {
            max_end = end;
        }
        let align = wire_align(ctx, ty);
        if align > max_align {
            max_align = align;
        }
    }
    // Round up to the struct's natural alignment so consecutive elements stay aligned.
    Ok(align_up(max_end, max_align.max(1)))
}

fn align_up(value: u32, align: u32) -> u32 {
    if align <= 1 {
        return value;
    }
    let rem = value % align;
    if rem == 0 { value } else { value + (align - rem) }
}

// ---------------------------------------------------------------------------
// Header (.h) and Source (.c) rendering.
// ---------------------------------------------------------------------------

fn render_header(ctx: &Context) -> String {
    let mut out = String::new();
    out.push_str(LICENSE_BANNER);
    out.push_str(AUTOGEN_BANNER);
    out.push_str("\n#ifndef _BLUEBERRY_DEVICES_MODULE_\n");
    out.push_str("#define _BLUEBERRY_DEVICES_MODULE_\n\n\n");

    out.push_str(SECTION_INCLUDES);
    out.push_str("\n#include <stdbool.h>\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include <blueberry-transcoder.h>\n\n");

    out.push_str(SECTION_DEFINES);
    out.push_str("\n//Message keys\n");
    let mut key_lines: Vec<(String, String)> = ctx
        .messages
        .iter()
        .map(|m| (header_message_key_macro(m), format!("(0x{:08x})", combined_key(m))))
        .collect();
    key_lines.sort_by(|a, b| a.0.cmp(&b.0));
    for (macro_name, literal) in &key_lines {
        let _ = writeln!(out, "#define {} {}", macro_name, literal);
    }
    out.push_str("//Numerical & Boolean Constants\n\n");

    out.push_str(SECTION_TYPES);
    out.push('\n');
    for enum_model in &ctx.enum_models {
        emit_enum_typedef(&mut out, enum_model);
    }
    out.push_str("\n");

    out.push_str(SECTION_VARIABLES);
    out.push_str("\n\n\n");

    out.push_str(SECTION_TOPICS);
    out.push('\n');
    out.push('\n');
    let mut topic_idents: Vec<String> = ctx
        .messages
        .iter()
        .map(|m| topic_macro_name(m))
        .collect();
    topic_idents.sort();
    for ident in &topic_idents {
        let _ = writeln!(out, "extern const char {}[];", ident);
    }
    out.push_str("\n\n");

    out.push_str(SECTION_PROTOTYPES);
    out.push('\n');
    out.push('\n');
    // Emit `add*` builders in alphabetical message-name order for stability.
    let mut sorted: Vec<&MessageModel> = ctx.messages.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for m in &sorted {
        emit_message_prototypes(&mut out, ctx, m);
    }

    out.push_str("\n#endif\n");
    out
}

fn render_source(ctx: &Context) -> String {
    let mut out = String::new();
    out.push_str(LICENSE_BANNER);
    out.push_str(AUTOGEN_BANNER);
    out.push('\n');
    out.push('\n');

    out.push_str(SECTION_INCLUDES);
    out.push_str("\n#include <blueberry_devices.h>\n");
    out.push_str("#include <blueberry-message.h>\n\n");

    out.push_str(SECTION_DEFINES);
    out.push('\n');

    // ---- field indices ----
    out.push_str("\n//Add message field indeces\n");
    let mut index_lines: Vec<String> = Vec::new();
    for m in &ctx.messages {
        collect_index_macros(m, &mut index_lines);
    }
    index_lines.sort();
    index_lines.dedup();
    for line in &index_lines {
        out.push_str(line);
        out.push('\n');
    }

    // ---- field ordinals ----
    out.push_str("\n//Add message field ordinals\n");
    let mut ordinal_lines: Vec<String> = Vec::new();
    for m in &ctx.messages {
        collect_ordinal_macros(m, &mut ordinal_lines);
    }
    ordinal_lines.sort();
    ordinal_lines.dedup();
    for line in &ordinal_lines {
        out.push_str(line);
        out.push('\n');
    }

    // ---- per-message key + max-ordinal ----
    out.push_str(
        "\n//Add message max ordinals - the number of fields in the message and the ordinal of the last field of the message\n",
    );
    let mut summary_lines: Vec<String> = Vec::new();
    for m in &ctx.messages {
        summary_lines.push(format!(
            "#define {}_MAX_ORDINAL ({})",
            screaming_message_token(&m.name),
            m.max_ordinal
        ));
        summary_lines.push(format!(
            "#define {}_MODULE_MESSAGE_KEY (0x{:08x})",
            screaming_message_token(&m.name),
            combined_key(m)
        ));
    }
    summary_lines.sort();
    for line in &summary_lines {
        out.push_str(line);
        out.push('\n');
    }

    // ---- bool field masks ----
    let mut mask_lines: Vec<String> = Vec::new();
    for m in &ctx.messages {
        collect_bool_mask_macros(m, &mut mask_lines);
    }
    if !mask_lines.is_empty() {
        out.push_str("\n//Add message boolean field masks\n");
        mask_lines.sort();
        mask_lines.dedup();
        for line in &mask_lines {
            out.push_str(line);
            out.push('\n');
        }
    }

    // ---- sequence element byte counts ----
    let mut seq_count_lines: Vec<String> = Vec::new();
    for m in &ctx.messages {
        collect_sequence_element_byte_counts(m, &mut seq_count_lines);
    }
    if !seq_count_lines.is_empty() {
        out.push_str("\n//Add sequence element byte count\n");
        seq_count_lines.sort();
        seq_count_lines.dedup();
        for line in &seq_count_lines {
            out.push_str(line);
            out.push('\n');
        }
    }

    // ---- message body lengths ----
    out.push_str("\n//Add message lengths - measured in bytes\n");
    let mut length_lines: Vec<String> = ctx
        .messages
        .iter()
        .map(|m| format!("#define {}_LENGTH ({})", screaming_message_token(&m.name), m.body_length))
        .collect();
    length_lines.sort();
    for line in &length_lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    out.push_str(SECTION_TYPES);
    out.push_str("\n\n\n");

    out.push_str(SECTION_VARIABLES);
    out.push_str("\n\n\n");

    out.push_str(SECTION_TOPICS);
    out.push('\n');
    out.push('\n');
    let mut topic_defs: Vec<String> = ctx
        .messages
        .iter()
        .map(|m| {
            format!(
                "const char {}[] = {};",
                topic_macro_name(m),
                quoted_topic(&m.topic)
            )
        })
        .collect();
    topic_defs.sort();
    for line in &topic_defs {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("\n");

    out.push_str(SECTION_PROTOTYPES);
    out.push_str("\n\n\n");
    out.push_str(SECTION_SOURCE);
    out.push('\n');
    out.push('\n');

    let mut sorted: Vec<&MessageModel> = ctx.messages.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for m in &sorted {
        emit_message_source(&mut out, ctx, m);
    }

    out
}

// ---------------------------------------------------------------------------
// Section banners that match the legacy `blueberry-schema-parser` template.
// ---------------------------------------------------------------------------

const LICENSE_BANNER: &str = r#"/*
 * Copyright (c) 2025 Blue Robotics North Inc.
 * 
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 * 
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * 
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

"#;

const AUTOGEN_BANNER: &str = "//*************************************************************************************\n//*************************************************************************************\n//ATTENTION! THIS FILE WAS AUTOGENERATED BY THE BLUEBERRY SCHEMA PARSER.\n//It's probably not a good idea to modify it. :-P\n//*************************************************************************************\n//*************************************************************************************\n";

const SECTION_INCLUDES: &str = "//*************************************************************************************\n//Includes\n//*************************************************************************************\n";
const SECTION_DEFINES: &str = "//*************************************************************************************\n//Defines\n//*************************************************************************************\n";
const SECTION_TYPES: &str = "//*************************************************************************************\n//Types\n//*************************************************************************************\n";
const SECTION_VARIABLES: &str = "//*************************************************************************************\n//Variables\n//*************************************************************************************\n";
const SECTION_TOPICS: &str = "//*************************************************************************************\n//Topic String Constants\n//*************************************************************************************\n";
const SECTION_PROTOTYPES: &str = "//*************************************************************************************\n//Function Prototypes\n//*************************************************************************************\n";
const SECTION_SOURCE: &str = "//*************************************************************************************\n//Source\n//*************************************************************************************\n";

// ---------------------------------------------------------------------------
// Enum, prototype, and source emission helpers.
// ---------------------------------------------------------------------------

fn emit_enum_typedef(out: &mut String, e: &EnumModel) {
    emit_comment_block(out, &e.comments, "");
    out.push_str("typedef enum {\n");
    for v in &e.values {
        let _ = writeln!(
            out,
            "\t{}_{} = {}, ",
            e.value_prefix, v.name, v.literal
        );
    }
    let _ = writeln!(out, "}} {};", e.typedef);
    out.push('\n');
}

fn emit_message_prototypes(out: &mut String, ctx: &Context, m: &MessageModel) {
    // ---- add<Message> ----
    emit_doc_add_builder(out, m);
    let signature = render_add_signature(ctx, m);
    let _ = writeln!(out, "{};", signature);

    // ---- is<Message>Empty / Full ----
    emit_doc_block(out, &["Tests if the current message has no fields present."], &m.comments);
    let _ = writeln!(out, "bool is{}Empty(Bb * buf, BbBlock msg);", m.name);
    emit_doc_block(out, &["Tests if the current message has all defined fields present."], &m.comments);
    let _ = writeln!(out, "bool is{}Full(Bb * buf, BbBlock msg);", m.name);

    // ---- per-field accessors ----
    for field in &m.fields {
        emit_field_prototypes(out, ctx, m, field);
    }
}

fn emit_field_prototypes(out: &mut String, ctx: &Context, m: &MessageModel, field: &MessageFieldModel) {
    match &field.kind {
        MessageFieldKind::Scalar(scalar) => {
            let return_ty = c_type_name(ctx, &scalar.ty);
            let accessor = format!("get{}{}", m.name, capitalize(&field.name));
            emit_doc_block(out, &[&format!("A getter for the {} field", field.name)], &field.comments);
            if is_bool_type(&scalar.ty) {
                let _ = writeln!(
                    out,
                    "bool is{}{}(Bb * buf, BbBlock msg );",
                    m.name,
                    capitalize(&field.name)
                );
            } else {
                let _ = writeln!(
                    out,
                    "{} {}(Bb * buf, BbBlock msg );",
                    return_ty, accessor
                );
            }
            emit_doc_block(
                out,
                &[&format!("Tests if the current message containts the {} field", field.name)],
                &field.comments,
            );
            let _ = writeln!(
                out,
                "bool is{}{}Present(Bb * buf, BbBlock msg );",
                m.name,
                capitalize(&field.name)
            );
        }
        MessageFieldKind::Sequence(seq) => {
            match &seq.element {
                SequenceElement::Struct(struct_model) => {
                    for sub in &struct_model.fields {
                        emit_sequence_sub_field_prototypes(out, ctx, m, field, sub);
                    }
                }
                SequenceElement::Primitive(prim) => {
                    let return_ty = c_type_name(ctx, prim);
                    let accessor = format!("get{}{}", m.name, capitalize(&field.name));
                    let setter = format!("set{}{}", m.name, capitalize(&field.name));
                    emit_doc_block(out, &[&format!("A getter for the {} field", field.name)], &field.comments);
                    let _ = writeln!(
                        out,
                        "{} {}(Bb * buf, BbBlock msg , uint32_t i0);",
                        return_ty, accessor
                    );
                    emit_doc_block(out, &[&format!("A setter for the {} field", field.name)], &field.comments);
                    let _ = writeln!(
                        out,
                        "void {}(Bb * buf, BbBlock msg , uint32_t i0, {} {});",
                        setter, return_ty, field.name
                    );
                }
            }
            emit_doc_block(
                out,
                &[&format!("A function to initialize a {} Sequence", titlecase_words(&field.name))],
                &field.comments,
            );
            let _ = writeln!(
                out,
                "void init{}{}(Bb * buf, BbBlock msg, uint32_t n);",
                m.name,
                capitalize(&field.name)
            );
            emit_doc_block(
                out,
                &[&format!(
                    "Gets the defined length of a sequence {} Sequence",
                    titlecase_words(&field.name)
                )],
                &field.comments,
            );
            let _ = writeln!(
                out,
                "uint32_t get{}{}SequenceLength(Bb * buf, BbBlock msg);",
                m.name,
                capitalize(&field.name)
            );
        }
    }
}

fn emit_sequence_sub_field_prototypes(
    out: &mut String,
    ctx: &Context,
    m: &MessageModel,
    field: &MessageFieldModel,
    sub: &StructFieldLayout,
) {
    let combined = format!("{}{}", capitalize(&field.name), capitalize(&sub.name));
    let return_ty = c_type_name(ctx, &sub.ty);
    emit_doc_block(
        out,
        &[
            &format!("A getter for the {} field", sub.name),
            &format!("@param buf - the message buffer to add the message to"),
            &format!("@param msg - the index of the start of the message"),
            &format!("@param i0 - index of {} sequence.", field.name),
        ],
        &sub.comments,
    );
    if is_bool_type(&sub.ty) {
        let _ = writeln!(
            out,
            "bool is{}{}(Bb * buf, BbBlock msg , uint32_t i0);",
            m.name, combined
        );
    } else {
        let _ = writeln!(
            out,
            "{} get{}{}(Bb * buf, BbBlock msg , uint32_t i0);",
            return_ty, m.name, combined
        );
    }
    emit_doc_block(
        out,
        &[
            &format!("A setter for the {} field", sub.name),
            &format!("@param buf - the message buffer to add the message to"),
            &format!("@param msg - the index of the start of the message"),
            &format!("@param i0 - index of {} sequence.", field.name),
            &format!("@param {}", sub.name),
        ],
        &sub.comments,
    );
    let _ = writeln!(
        out,
        "void set{}{}(Bb * buf, BbBlock msg , uint32_t i0, {} {});",
        m.name, combined, return_ty, sub.name
    );
}

fn render_add_signature(ctx: &Context, m: &MessageModel) -> String {
    let mut params: Vec<String> = vec!["Bb * buf".to_string()];
    for f in &m.fields {
        if let MessageFieldKind::Scalar(s) = &f.kind {
            let ty = c_type_name(ctx, &s.ty);
            params.push(format!("{} {}", ty, f.name));
        }
    }
    format!("BbBlock add{}({})", m.name, params.join(", "))
}

fn emit_message_source(out: &mut String, ctx: &Context, m: &MessageModel) {
    // add builder
    emit_doc_add_builder(out, m);
    let signature = render_add_signature(ctx, m);
    let _ = writeln!(out, "{}{{", signature);
    let token = screaming_message_token(&m.name);
    out.push_str("\tBbBlock msg = buf->length;\n");
    out.push_str("\t//Extend buffer to include the main message body before writing it\n");
    let _ = writeln!(out, "\tbuf->length = msg + {}_LENGTH;", token);
    let _ = writeln!(
        out,
        "\tsetBbUint32(buf, msg, {tok}_MODULE_MESSAGE_KEY_INDEX, {tok}_MODULE_MESSAGE_KEY);",
        tok = token
    );
    let _ = writeln!(
        out,
        "\tsetBbUint16(buf, msg, {tok}_LENGTH_INDEX, {tok}_LENGTH/4);//length field is measured in 4-byte words",
        tok = token
    );
    let _ = writeln!(
        out,
        "\tsetBbUint8(buf, msg, {tok}_MAX_ORDINAL_INDEX, {tok}_MAX_ORDINAL);",
        tok = token
    );

    for f in &m.fields {
        match &f.kind {
            MessageFieldKind::Scalar(s) => {
                emit_scalar_setter_in_add(out, ctx, m, f, &s.ty);
            }
            MessageFieldKind::Sequence(_) => {
                let _ = writeln!(
                    out,
                    "\tsetBbUint16(buf, msg, {tok}_{f}_PLACEHOLDER_INDEX, BB_INVALID_BLOCK);//clear sequence header",
                    tok = token,
                    f = to_screaming_snake(&f.name)
                );
            }
        }
    }
    out.push_str("\treturn msg;\n");
    out.push_str("}\n");

    // isEmpty / isFull
    emit_doc_block(out, &["Tests if the current message has no fields present."], &m.comments);
    let _ = writeln!(out, "bool is{}Empty(Bb * buf, BbBlock msg){{", m.name);
    out.push_str("\treturn getBbMessageMaxOrdinal(buf, msg) <= 2;//will always be length and ordinal fields\n");
    out.push_str("}\n");
    emit_doc_block(out, &["Tests if the current message has all defined fields present."], &m.comments);
    let _ = writeln!(out, "bool is{}Full(Bb * buf, BbBlock msg){{", m.name);
    let _ = writeln!(
        out,
        "\treturn getBbMessageMaxOrdinal(buf, msg) >= {}_MAX_ORDINAL;",
        token
    );
    out.push_str("}\n");

    // per-field accessors
    for f in &m.fields {
        emit_field_source(out, ctx, m, f);
    }
}

fn emit_scalar_setter_in_add(
    out: &mut String,
    ctx: &Context,
    m: &MessageModel,
    field: &MessageFieldModel,
    ty: &Type,
) {
    let token = screaming_message_token(&m.name);
    let field_tok = to_screaming_snake(&field.name);
    let resolved = enum_or_scalar_base(ctx, ty);
    match resolved {
        ResolvedScalar::Bool => {
            let _ = writeln!(
                out,
                "\tsetBbBool(buf, msg, {tok}_{f}_INDEX, {mask}, {name});",
                tok = token,
                f = field_tok,
                mask = bool_mask_token(m, field),
                name = field.name
            );
        }
        ResolvedScalar::Uint8 | ResolvedScalar::Char | ResolvedScalar::Octet => {
            let _ = writeln!(
                out,
                "\tsetBbUint8(buf, msg, {tok}_{f}_INDEX, {name});",
                tok = token,
                f = field_tok,
                name = field.name
            );
        }
        ResolvedScalar::Uint16 | ResolvedScalar::Int16 => {
            let _ = writeln!(
                out,
                "\tsetBbUint16(buf, msg, {tok}_{f}_INDEX, {name});",
                tok = token,
                f = field_tok,
                name = field.name
            );
        }
        ResolvedScalar::Uint32 | ResolvedScalar::Int32 => {
            let _ = writeln!(
                out,
                "\tsetBbUint32(buf, msg, {tok}_{f}_INDEX, {name});",
                tok = token,
                f = field_tok,
                name = field.name
            );
        }
        ResolvedScalar::Float => {
            let _ = writeln!(
                out,
                "\tsetBbFloat32(buf, msg, {tok}_{f}_INDEX, {name});",
                tok = token,
                f = field_tok,
                name = field.name
            );
        }
        // 64-bit + string types intentionally use the closest available helper
        // (firmware-side definitions of `setBbUint64` / `copyBbStringToMessage`)
        // until the legacy parser's exact wire layout for them is verified.
        ResolvedScalar::Uint64 | ResolvedScalar::Int64 => {
            let _ = writeln!(
                out,
                "\tsetBbUint64(buf, msg, {tok}_{f}_INDEX, {name});",
                tok = token,
                f = field_tok,
                name = field.name
            );
        }
        ResolvedScalar::Double => {
            let _ = writeln!(
                out,
                "\tsetBbDouble(buf, msg, {tok}_{f}_INDEX, {name});",
                tok = token,
                f = field_tok,
                name = field.name
            );
        }
        ResolvedScalar::StringBounded(_) => {
            // String fields are not produced by `add<Message>` directly — callers
            // populate them with `copyBbStringToMessage` later.
            let _ = writeln!(
                out,
                "\t// {} is a bounded string; populate with copyBbStringToMessage",
                field.name
            );
        }
    }
}

fn emit_field_source(out: &mut String, ctx: &Context, m: &MessageModel, field: &MessageFieldModel) {
    match &field.kind {
        MessageFieldKind::Scalar(s) => {
            emit_scalar_getter(out, ctx, m, field, &s.ty);
        }
        MessageFieldKind::Sequence(seq) => {
            emit_sequence_source(out, ctx, m, field, seq);
        }
    }
}

fn emit_scalar_getter(
    out: &mut String,
    ctx: &Context,
    m: &MessageModel,
    field: &MessageFieldModel,
    ty: &Type,
) {
    emit_doc_block(out, &[&format!("A getter for the {} field", field.name)], &field.comments);
    let token = screaming_message_token(&m.name);
    let field_tok = to_screaming_snake(&field.name);
    let resolved = enum_or_scalar_base(ctx, ty);
    let return_ty = c_type_name(ctx, ty);
    let accessor = format!("get{}{}", m.name, capitalize(&field.name));
    match resolved {
        ResolvedScalar::Bool => {
            let _ = writeln!(
                out,
                "bool is{}{}(Bb * buf, BbBlock msg ){{",
                m.name,
                capitalize(&field.name)
            );
            out.push_str("\tuint16_t i = 0;\n");
            let _ = writeln!(out, "\ti += {}_{}_INDEX;", token, field_tok);
            let _ = writeln!(
                out,
                "\treturn getBbBool(buf, msg, i, {});",
                bool_mask_token(m, field)
            );
            out.push_str("}\n");
        }
        ResolvedScalar::Uint8 | ResolvedScalar::Char | ResolvedScalar::Octet => {
            emit_simple_scalar_getter(out, &accessor, &return_ty, &token, &field_tok, "getBbUint8");
        }
        ResolvedScalar::Uint16 | ResolvedScalar::Int16 => {
            emit_simple_scalar_getter(out, &accessor, &return_ty, &token, &field_tok, "getBbUint16");
        }
        ResolvedScalar::Uint32 | ResolvedScalar::Int32 => {
            emit_simple_scalar_getter(out, &accessor, &return_ty, &token, &field_tok, "getBbUint32");
        }
        ResolvedScalar::Float => {
            emit_simple_scalar_getter(out, &accessor, &return_ty, &token, &field_tok, "getBbFloat32");
        }
        ResolvedScalar::Uint64 | ResolvedScalar::Int64 => {
            emit_simple_scalar_getter(out, &accessor, &return_ty, &token, &field_tok, "getBbUint64");
        }
        ResolvedScalar::Double => {
            emit_simple_scalar_getter(out, &accessor, &return_ty, &token, &field_tok, "getBbDouble");
        }
        ResolvedScalar::StringBounded(_) => {
            // No direct getter — caller uses `copyBbStringFromMessage`.
        }
    }
    emit_doc_block(
        out,
        &[&format!("Tests if the current message containts the {} field", field.name)],
        &field.comments,
    );
    let _ = writeln!(
        out,
        "bool is{}{}Present(Bb * buf, BbBlock msg ){{",
        m.name,
        capitalize(&field.name)
    );
    let _ = writeln!(
        out,
        "\treturn {}_{}_ORDINAL <= (getBbMessageMaxOrdinal(buf, msg));",
        token, field_tok
    );
    out.push_str("}\n");
}

fn emit_simple_scalar_getter(
    out: &mut String,
    accessor: &str,
    return_ty: &str,
    msg_token: &str,
    field_tok: &str,
    helper: &str,
) {
    let _ = writeln!(out, "{} {}(Bb * buf, BbBlock msg ){{", return_ty, accessor);
    out.push_str("\tuint16_t i = 0;\n");
    let _ = writeln!(out, "\ti += {}_{}_INDEX;", msg_token, field_tok);
    let _ = writeln!(out, "\treturn {}(buf, msg, i);", helper);
    out.push_str("}\n");
}

fn emit_sequence_source(
    out: &mut String,
    ctx: &Context,
    m: &MessageModel,
    field: &MessageFieldModel,
    seq: &SequenceSpec,
) {
    let msg_token = screaming_message_token(&m.name);
    let field_tok = to_screaming_snake(&field.name);
    match &seq.element {
        SequenceElement::Struct(struct_model) => {
            for sub in &struct_model.fields {
                emit_sequence_sub_field_source(out, ctx, m, field, sub);
            }
        }
        SequenceElement::Primitive(prim) => {
            emit_sequence_primitive_source(out, ctx, m, field, prim);
        }
    }
    // init
    emit_doc_block(
        out,
        &[&format!("A function to initialize a {} Sequence", titlecase_words(&field.name))],
        &field.comments,
    );
    let _ = writeln!(
        out,
        "void init{}{}(Bb * buf, BbBlock msg, uint32_t n){{",
        m.name,
        capitalize(&field.name)
    );
    out.push_str("\tuint16_t i = 0;\n");
    let _ = writeln!(out, "\ti += {}_{}_PLACEHOLDER_INDEX;", msg_token, field_tok);
    out.push_str("\tif(isBbBlockInvalid(i)){\n");
    out.push_str("\t\treturn;//bail because a sequence was not initialized\n");
    out.push_str("\t}\n");
    out.push_str("\t//i is now the index of this sequence field header\n");
    let _ = writeln!(
        out,
        "\tuint32_t bs = {}_{}_PLACEHOLDER_SEQUENCE_ELEMENT_BYTE_COUNT; //the 4 is to account for the length field that precedes the sequence data",
        msg_token, field_tok
    );
    out.push_str("\tinitBbSequence(buf, msg, i, bs, n);\n");
    out.push_str("}\n");

    // length
    emit_doc_block(
        out,
        &[&format!("Gets the defined length of a sequence {} Sequence", titlecase_words(&field.name))],
        &field.comments,
    );
    let _ = writeln!(
        out,
        "uint32_t get{}{}SequenceLength(Bb * buf, BbBlock msg){{",
        m.name,
        capitalize(&field.name)
    );
    out.push_str("\tuint16_t i = 0;\n");
    let _ = writeln!(out, "\ti += {}_{}_PLACEHOLDER_INDEX;", msg_token, field_tok);
    out.push_str("\tif(isBbBlockInvalid(i)){\n");
    out.push_str("\t\treturn 0;//bail because a sequence was not initialized\n");
    out.push_str("\t}\n");
    out.push_str("\t//i is now the index of this sequence field header\n");
    out.push_str("\treturn getBbSequenceLength(buf, msg, i);\n");
    out.push_str("}\n");
}

fn emit_sequence_sub_field_source(
    out: &mut String,
    ctx: &Context,
    m: &MessageModel,
    field: &MessageFieldModel,
    sub: &StructFieldLayout,
) {
    let msg_token = screaming_message_token(&m.name);
    let field_tok = to_screaming_snake(&field.name);
    let sub_tok = to_screaming_snake(&sub.name);
    let combined = format!("{}{}", capitalize(&field.name), capitalize(&sub.name));
    let return_ty = c_type_name(ctx, &sub.ty);
    let resolved = enum_or_scalar_base(ctx, &sub.ty);

    let (helper_get, helper_set) = helper_pair(&resolved);
    emit_doc_block(
        out,
        &[
            &format!("A getter for the {} field", sub.name),
            &format!("@param buf - the message buffer to add the message to"),
            &format!("@param msg - the index of the start of the message"),
            &format!("@param i0 - index of {} sequence.", field.name),
        ],
        &sub.comments,
    );
    let getter_name = if matches!(resolved, ResolvedScalar::Bool) {
        format!("is{}{}", m.name, combined)
    } else {
        format!("get{}{}", m.name, combined)
    };
    let return_signature = if matches!(resolved, ResolvedScalar::Bool) { "bool" } else { return_ty.as_str() };
    let _ = writeln!(
        out,
        "{} {}(Bb * buf, BbBlock msg , uint32_t i0){{",
        return_signature, getter_name
    );
    out.push_str("\tuint16_t i = 0;\n");
    let _ = writeln!(out, "\ti += {}_{}_PLACEHOLDER_INDEX;", msg_token, field_tok);
    out.push_str("\ti = getBbSequenceElementIndex(buf, msg, i, i0);\n");
    let _ = writeln!(
        out,
        "\ti += {}_{}_{}_INDEX;",
        msg_token, field_tok, sub_tok
    );
    if matches!(resolved, ResolvedScalar::Bool) {
        let _ = writeln!(
            out,
            "\treturn getBbBool(buf, msg, i, {});",
            bool_mask_token_for_sub(m, field, sub)
        );
    } else {
        let _ = writeln!(out, "\treturn {}(buf, msg, i);", helper_get);
    }
    out.push_str("}\n");

    // setter
    emit_doc_block(
        out,
        &[
            &format!("A setter for the {} field", sub.name),
            &format!("@param buf - the message buffer to add the message to"),
            &format!("@param msg - the index of the start of the message"),
            &format!("@param i0 - index of {} sequence.", field.name),
            &format!("@param {}", sub.name),
        ],
        &sub.comments,
    );
    let _ = writeln!(
        out,
        "void set{}{}(Bb * buf, BbBlock msg , uint32_t i0, {} {}){{",
        m.name, combined, return_ty, sub.name
    );
    out.push_str("\tuint16_t i = 0;\n");
    let _ = writeln!(out, "\ti += {}_{}_PLACEHOLDER_INDEX;", msg_token, field_tok);
    out.push_str("\ti = getBbSequenceElementIndex(buf, msg, i, i0);\n");
    let _ = writeln!(
        out,
        "\ti += {}_{}_{}_INDEX;",
        msg_token, field_tok, sub_tok
    );
    out.push_str("\tif(isBbBlockInvalid(i)){\n");
    out.push_str("\t\treturn;//bail because a sequence was not initialized\n");
    out.push_str("\t}\n");
    if matches!(resolved, ResolvedScalar::Bool) {
        let _ = writeln!(
            out,
            "\tsetBbBool(buf, msg, i, {}, {});",
            bool_mask_token_for_sub(m, field, sub),
            sub.name
        );
    } else {
        let _ = writeln!(out, "\t{}(buf, msg, i, {});", helper_set, sub.name);
    }
    out.push_str("}\n");
}

fn emit_sequence_primitive_source(
    out: &mut String,
    ctx: &Context,
    m: &MessageModel,
    field: &MessageFieldModel,
    prim: &Type,
) {
    let msg_token = screaming_message_token(&m.name);
    let field_tok = to_screaming_snake(&field.name);
    let return_ty = c_type_name(ctx, prim);
    let accessor = format!("get{}{}", m.name, capitalize(&field.name));
    let setter = format!("set{}{}", m.name, capitalize(&field.name));
    let resolved = enum_or_scalar_base(ctx, prim);
    let (helper_get, helper_set) = helper_pair(&resolved);

    emit_doc_block(
        out,
        &[
            &format!("A getter for the {} field", field.name),
            &format!("@param buf - the message buffer to add the message to"),
            &format!("@param msg - the index of the start of the message"),
            &format!("@param i0 - index of {} sequence.", field.name),
        ],
        &field.comments,
    );
    let _ = writeln!(
        out,
        "{} {}(Bb * buf, BbBlock msg , uint32_t i0){{",
        return_ty, accessor
    );
    out.push_str("\tuint16_t i = 0;\n");
    let _ = writeln!(out, "\ti += {}_{}_PLACEHOLDER_INDEX;", msg_token, field_tok);
    out.push_str("\ti = getBbSequenceElementIndex(buf, msg, i, i0);\n");
    let _ = writeln!(out, "\ti += {}_{}_INDEX;", msg_token, field_tok);
    let _ = writeln!(out, "\treturn {}(buf, msg, i);", helper_get);
    out.push_str("}\n");

    emit_doc_block(
        out,
        &[
            &format!("A setter for the {} field", field.name),
            &format!("@param buf - the message buffer to add the message to"),
            &format!("@param msg - the index of the start of the message"),
            &format!("@param i0 - index of {} sequence.", field.name),
            &format!("@param {}", field.name),
        ],
        &field.comments,
    );
    let _ = writeln!(
        out,
        "void {}(Bb * buf, BbBlock msg , uint32_t i0, {} {}){{",
        setter, return_ty, field.name
    );
    out.push_str("\tuint16_t i = 0;\n");
    let _ = writeln!(out, "\ti += {}_{}_PLACEHOLDER_INDEX;", msg_token, field_tok);
    out.push_str("\ti = getBbSequenceElementIndex(buf, msg, i, i0);\n");
    let _ = writeln!(out, "\ti += {}_{}_INDEX;", msg_token, field_tok);
    out.push_str("\tif(isBbBlockInvalid(i)){\n");
    out.push_str("\t\treturn;//bail because a sequence was not initialized\n");
    out.push_str("\t}\n");
    let _ = writeln!(out, "\t{}(buf, msg, i, {});", helper_set, field.name);
    out.push_str("}\n");
}

// ---------------------------------------------------------------------------
// Macro collection helpers (.c file `#define` blocks).
// ---------------------------------------------------------------------------

fn collect_index_macros(m: &MessageModel, out: &mut Vec<String>) {
    let msg_token = screaming_message_token(&m.name);
    // Header field indices (module-message-key, length, max-ordinal) are constant.
    out.push(format!("#define {}_MODULE_MESSAGE_KEY_INDEX (0)", msg_token));
    out.push(format!("#define {}_LENGTH_INDEX (4)", msg_token));
    out.push(format!("#define {}_MAX_ORDINAL_INDEX (6)", msg_token));

    for f in &m.fields {
        match &f.kind {
            MessageFieldKind::Scalar(_) => {
                out.push(format!(
                    "#define {}_{}_INDEX ({})",
                    msg_token,
                    to_screaming_snake(&f.name),
                    f.offset
                ));
            }
            MessageFieldKind::Sequence(seq) => {
                out.push(format!(
                    "#define {}_{}_PLACEHOLDER_INDEX ({})",
                    msg_token,
                    to_screaming_snake(&f.name),
                    f.offset
                ));
                match &seq.element {
                    SequenceElement::Struct(struct_model) => {
                        for sub in &struct_model.fields {
                            out.push(format!(
                                "#define {}_{}_{}_INDEX ({})",
                                msg_token,
                                to_screaming_snake(&f.name),
                                to_screaming_snake(&sub.name),
                                sub.offset
                            ));
                        }
                    }
                    SequenceElement::Primitive(_) => {
                        // Primitive sequences expose a `_<FIELD>_INDEX (0)` macro for the
                        // (single) sub-element so the accessor body matches the struct case.
                        out.push(format!(
                            "#define {}_{}_DATA_INDEX (0)",
                            msg_token,
                            to_screaming_snake(&f.name)
                        ));
                    }
                }
            }
        }
    }
}

fn collect_ordinal_macros(m: &MessageModel, out: &mut Vec<String>) {
    let msg_token = screaming_message_token(&m.name);
    out.push(format!("#define {}_MODULE_MESSAGE_KEY_ORDINAL (0)", msg_token));
    out.push(format!("#define {}_LENGTH_ORDINAL (1)", msg_token));
    out.push(format!("#define {}_MAX_ORDINAL_ORDINAL (2)", msg_token));
    for f in &m.fields {
        match &f.kind {
            MessageFieldKind::Scalar(_) => {
                out.push(format!(
                    "#define {}_{}_ORDINAL ({})",
                    msg_token,
                    to_screaming_snake(&f.name),
                    f.ordinal
                ));
            }
            MessageFieldKind::Sequence(seq) => {
                out.push(format!(
                    "#define {}_{}_PLACEHOLDER_ORDINAL ({})",
                    msg_token,
                    to_screaming_snake(&f.name),
                    f.ordinal
                ));
                match &seq.element {
                    SequenceElement::Struct(struct_model) => {
                        for sub in &struct_model.fields {
                            out.push(format!(
                                "#define {}_{}_{}_ORDINAL ({})",
                                msg_token,
                                to_screaming_snake(&f.name),
                                to_screaming_snake(&sub.name),
                                f.ordinal
                            ));
                        }
                    }
                    SequenceElement::Primitive(_) => {
                        out.push(format!(
                            "#define {}_{}_DATA_ORDINAL ({})",
                            msg_token,
                            to_screaming_snake(&f.name),
                            f.ordinal
                        ));
                    }
                }
            }
        }
    }
}

fn collect_bool_mask_macros(m: &MessageModel, out: &mut Vec<String>) {
    let msg_token = screaming_message_token(&m.name);
    for f in &m.fields {
        match &f.kind {
            MessageFieldKind::Scalar(s) if matches!(s.ty, Type::Boolean) => {
                out.push(format!(
                    "#define {}_{}_MASK {}",
                    msg_token,
                    to_screaming_snake(&f.name),
                    BOOL_MASK_BIT0
                ));
            }
            MessageFieldKind::Sequence(seq) => {
                if let SequenceElement::Struct(struct_model) = &seq.element {
                    for sub in &struct_model.fields {
                        if matches!(sub.ty, Type::Boolean) {
                            out.push(format!(
                                "#define {}_{}_{}_MASK {}",
                                msg_token,
                                to_screaming_snake(&f.name),
                                to_screaming_snake(&sub.name),
                                BOOL_MASK_BIT0
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_sequence_element_byte_counts(m: &MessageModel, out: &mut Vec<String>) {
    let msg_token = screaming_message_token(&m.name);
    for f in &m.fields {
        if let MessageFieldKind::Sequence(seq) = &f.kind {
            let size = match &seq.element {
                SequenceElement::Struct(struct_model) => struct_model.element_size,
                SequenceElement::Primitive(prim) => primitive_element_byte_count(prim),
            };
            out.push(format!(
                "#define {}_{}_PLACEHOLDER_SEQUENCE_ELEMENT_BYTE_COUNT ({})",
                msg_token,
                to_screaming_snake(&f.name),
                size
            ));
        }
    }
}

fn primitive_element_byte_count(ty: &Type) -> u32 {
    match ty {
        Type::Boolean | Type::Char | Type::Octet => 1,
        Type::Short | Type::UnsignedShort => 2,
        Type::Long | Type::UnsignedLong | Type::Float => 4,
        Type::LongLong | Type::UnsignedLongLong | Type::Double => 8,
        _ => 4,
    }
}

// ---------------------------------------------------------------------------
// Misc small helpers.
// ---------------------------------------------------------------------------

fn header_message_key_macro(m: &MessageModel) -> String {
    let module_path = m
        .module_path
        .iter()
        .map(|s| to_screaming_snake(s))
        .collect::<Vec<_>>()
        .join("_");
    let msg = to_screaming_snake(&m.name);
    let join = if module_path.is_empty() {
        msg
    } else {
        format!("{}_{}", module_path, msg)
    };
    format!("{}_KEY", join)
}

fn topic_macro_name(m: &MessageModel) -> String {
    format!("{}_TOPIC", to_screaming_snake(&m.name))
}

fn screaming_message_token(name: &str) -> String {
    to_screaming_snake(name)
}

fn combined_key(m: &MessageModel) -> u32 {
    ((m.module_key as u32) << 16) | (m.message_key as u32)
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn titlecase_words(s: &str) -> String {
    // Convert camelCase `foo` -> "Foo", `fooBarBaz` -> "Foo Bar Baz".
    let snake = to_snake_case(s);
    snake
        .split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn to_snake_case(identifier: &str) -> String {
    let mut result = String::new();
    let mut prev_lower = false;
    for ch in identifier.chars() {
        if ch.is_ascii_uppercase() {
            if (prev_lower) && !result.ends_with('_') {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else if ch == ' ' || ch == '-' {
            if !result.ends_with('_') {
                result.push('_');
            }
            prev_lower = false;
        } else if ch.is_ascii_digit() {
            // The legacy generator inserts an `_` between a lowercase letter
            // and a digit (e.g. `c0` -> `C_0`) but leaves digits adjacent to
            // uppercase / digit characters alone (e.g. `STM32F446` stays put).
            if prev_lower && !result.ends_with('_') {
                result.push('_');
            }
            result.push(ch);
            prev_lower = false;
        } else {
            result.push(ch.to_ascii_lowercase());
            prev_lower = ch.is_ascii_alphabetic() && ch.is_ascii_lowercase();
        }
    }
    result
}

fn to_screaming_snake(s: &str) -> String {
    to_snake_case(s).to_ascii_uppercase()
}

fn enum_typedef_name(name: &str) -> String {
    if name.ends_with("Enum") {
        name.to_string()
    } else {
        format!("{}Enum", name)
    }
}

fn c_type_name(ctx: &Context, ty: &Type) -> String {
    match ty {
        Type::Boolean => "bool".into(),
        Type::Char => "int8_t".into(),
        Type::Octet => "uint8_t".into(),
        Type::Short => "int16_t".into(),
        Type::UnsignedShort => "uint16_t".into(),
        Type::Long => "int32_t".into(),
        Type::UnsignedLong => "uint32_t".into(),
        Type::LongLong => "int64_t".into(),
        Type::UnsignedLongLong => "uint64_t".into(),
        Type::Float => "float".into(),
        Type::Double => "double".into(),
        Type::LongDouble => "long double".into(),
        Type::WChar => "uint16_t".into(),
        Type::String { .. } => "char *".into(),
        Type::WString => "uint16_t *".into(),
        Type::ScopedName(path) => {
            if let Some(def) = ctx.enums.get(path) {
                enum_typedef_name(&def.name)
            } else if let Some(last) = path.last() {
                last.clone()
            } else {
                "void".into()
            }
        }
        Type::Sequence { .. } | Type::Array { .. } => "void *".into(),
    }
}

fn is_bool_type(ty: &Type) -> bool {
    matches!(ty, Type::Boolean)
}

#[derive(Clone)]
enum ResolvedScalar {
    Bool,
    Char,
    Octet,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Int64,
    Uint64,
    Float,
    Double,
    StringBounded(u32),
}

fn enum_or_scalar_base(ctx: &Context, ty: &Type) -> ResolvedScalar {
    match ty {
        Type::Boolean => ResolvedScalar::Bool,
        Type::Char => ResolvedScalar::Char,
        Type::Octet => ResolvedScalar::Octet,
        Type::Short => ResolvedScalar::Int16,
        Type::UnsignedShort => ResolvedScalar::Uint16,
        Type::Long => ResolvedScalar::Int32,
        Type::UnsignedLong => ResolvedScalar::Uint32,
        Type::LongLong => ResolvedScalar::Int64,
        Type::UnsignedLongLong => ResolvedScalar::Uint64,
        Type::Float => ResolvedScalar::Float,
        Type::Double => ResolvedScalar::Double,
        Type::ScopedName(path) => {
            if let Some(def) = ctx.enums.get(path) {
                let base = def.base_type.clone().unwrap_or(Type::UnsignedLong);
                enum_or_scalar_base(ctx, &base)
            } else {
                ResolvedScalar::Uint32
            }
        }
        Type::String { bound: Some(n) } => ResolvedScalar::StringBounded(*n),
        _ => ResolvedScalar::Uint32,
    }
}

fn helper_pair(resolved: &ResolvedScalar) -> (&'static str, &'static str) {
    match resolved {
        ResolvedScalar::Bool => ("getBbBool", "setBbBool"),
        ResolvedScalar::Uint8 | ResolvedScalar::Octet | ResolvedScalar::Char => {
            ("getBbUint8", "setBbUint8")
        }
        ResolvedScalar::Uint16 | ResolvedScalar::Int16 => ("getBbUint16", "setBbUint16"),
        ResolvedScalar::Uint32 | ResolvedScalar::Int32 => ("getBbUint32", "setBbUint32"),
        ResolvedScalar::Uint64 | ResolvedScalar::Int64 => ("getBbUint64", "setBbUint64"),
        ResolvedScalar::Float => ("getBbFloat32", "setBbFloat32"),
        ResolvedScalar::Double => ("getBbDouble", "setBbDouble"),
        ResolvedScalar::StringBounded(_) => ("copyBbStringFromMessage", "copyBbStringToMessage"),
    }
}

fn bool_mask_token(m: &MessageModel, field: &MessageFieldModel) -> String {
    format!(
        "{}_{}_MASK",
        screaming_message_token(&m.name),
        to_screaming_snake(&field.name)
    )
}

fn bool_mask_token_for_sub(m: &MessageModel, field: &MessageFieldModel, sub: &StructFieldLayout) -> String {
    format!(
        "{}_{}_{}_MASK",
        screaming_message_token(&m.name),
        to_screaming_snake(&field.name),
        to_screaming_snake(&sub.name)
    )
}

// ---------------------------------------------------------------------------
// Doc / comment helpers.
// ---------------------------------------------------------------------------

fn emit_comment_block(out: &mut String, primary: &[String], indent: &str) {
    if primary.is_empty() {
        return;
    }
    out.push_str(indent);
    out.push_str("/**\n");
    for line in primary {
        let _ = writeln!(out, "{} * {}", indent, clean_doc_line(line));
    }
    out.push_str(indent);
    out.push_str(" */\n");
}

/// Strip leading `*` / whitespace from a raw block-comment line so we can
/// safely re-prefix it with ` * ` without producing `* *`.
fn clean_doc_line(line: &str) -> &str {
    let trimmed = line.trim_start();
    let trimmed = trimmed.strip_prefix('*').unwrap_or(trimmed);
    let trimmed = trimmed.strip_prefix(' ').unwrap_or(trimmed);
    trimmed.trim_end()
}

fn emit_doc_block(out: &mut String, lines: &[&str], extra: &[String]) {
    out.push_str("/**\n");
    for line in lines {
        let _ = writeln!(out, " * {}", clean_doc_line(line));
    }
    for line in extra {
        let _ = writeln!(out, " * {}", clean_doc_line(line));
    }
    out.push_str(" */\n");
}

fn emit_doc_add_builder(out: &mut String, m: &MessageModel) {
    out.push_str("/**\n");
    let _ = writeln!(
        out,
        " * Adds a {} to the end of the current buffer",
        titlecase_words(&m.name)
    );
    for line in &m.comments {
        let _ = writeln!(out, " * {}", clean_doc_line(line));
    }
    out.push_str(" * @param buf - the message buffer to add the message to\n");
    for f in &m.fields {
        if let MessageFieldKind::Scalar(_) = f.kind {
            out.push_str(" * @param ");
            out.push_str(&f.name);
            if let Some(first) = f.comments.first() {
                let _ = writeln!(out, " - {}", clean_doc_line(first));
            } else {
                out.push('\n');
            }
        }
    }
    out.push_str(" * @returns - the index of the new message.\n");
    out.push_str(" */\n");
}

fn quoted_topic(topic: &str) -> String {
    // The legacy parser substitutes `{device_type}` and `{nid}` with escape
    // sequences that the runtime fills in. Keep that behaviour byte-for-byte.
    let mut out = String::from("\"");
    let mut chars = topic.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut placeholder = String::new();
            while let Some(&c) = chars.peek() {
                chars.next();
                if c == '}' {
                    break;
                }
                placeholder.push(c);
            }
            match placeholder.as_str() {
                "device_type" => out.push_str("\\x81"),
                "nid" => out.push_str("\\x80"),
                other => {
                    out.push('{');
                    out.push_str(other);
                    out.push('}');
                }
            }
        } else if ch == '"' {
            out.push_str("\\\"");
        } else if ch == '\\' {
            out.push_str("\\\\");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

fn annotation_string(annotations: &[Annotation], name: &str) -> Option<String> {
    annotation_value(annotations, name).and_then(|v| match v {
        ConstValue::String(s) => Some(s.clone()),
        _ => None,
    })
}

fn annotation_u16(annotations: &[Annotation], name: &str) -> Option<u16> {
    annotation_value(annotations, name).and_then(|v| match v {
        ConstValue::Integer(lit) if (0..=u16::MAX as i128).contains(&lit.value) => {
            Some(lit.value as u16)
        }
        _ => None,
    })
}

fn annotation_value<'a>(annotations: &'a [Annotation], name: &str) -> Option<&'a ConstValue> {
    annotations
        .iter()
        .find(|a| {
            a.name
                .last()
                .map(|s| s.eq_ignore_ascii_case(name))
                .unwrap_or(false)
        })
        .and_then(|a| {
            a.params.iter().next().map(|p| match p {
                AnnotationParam::Named { value, .. } | AnnotationParam::Positional(value) => value,
            })
        })
}

fn enum_value_literal(member: &EnumMember, _base: &Type) -> String {
    let Some(value) = &member.value else {
        return "0".to_string();
    };
    match value {
        ConstValue::Integer(lit) => {
            // Match the legacy generator's emission: hex with at least 4 digits
            // and a leading `0x`.
            let v = lit.value as i128;
            if v >= 0 {
                format!("0x{:04x}", v as u128)
            } else {
                format!("{}", v)
            }
        }
        ConstValue::ScopedName(path) => path.join("_"),
        ConstValue::UnaryOp { op, expr } => {
            let inner = enum_value_literal(
                &EnumMember {
                    name: String::new(),
                    value: Some((**expr).clone()),
                    comments: Vec::new(),
                },
                _base,
            );
            match op {
                blueberry_ast::UnaryOperator::Plus => inner,
                blueberry_ast::UnaryOperator::Minus => format!("-{}", inner),
            }
        }
        _ => "0".to_string(),
    }
}

fn scoped(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", scope.join("::"), name)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use blueberry_parser::parse_idl;

    fn parse(src: &str) -> Vec<Definition> {
        parse_idl(src).expect("parse")
    }

    #[test]
    fn version_message_matches_legacy_shape() {
        let src = r#"
            @module_key(0x4244)
            module ::Blueberry::Devices {
                enum HwType : uint16 {
                    UNDEFINED = 0xffff,
                    LEGACY = 0x0000,
                    BLUE_SERVO = 0x0001
                };
                enum McuType : uint8 {
                    UNDEFINED = 0xff,
                    STM32F446 = 0x01
                };
                @topic("blueberry/devices/{device_type}/{nid}/version")
                @message_key(0x8366)
                message VersionMessage {
                    uint32 firmwareVersion;
                    uint8 hardwareRev;
                    HwType hardwareType;
                    McuType mcuType;
                };
            };
        "#;
        let defs = parse(src);
        let files = generate(&defs).unwrap();
        assert_eq!(files.len(), 2);
        let header = &files[0].contents;
        let source = &files[1].contents;

        // Header shape — message-key macro uses double-segment module path
        // and the legacy-style 32-bit combined key.
        assert!(
            header.contains("#define BLUEBERRY_DEVICES_VERSION_MESSAGE_KEY (0x42448366)"),
            "missing legacy-style VERSION_MESSAGE_KEY in header"
        );
        // Enum typedefs end with `Enum` suffix.
        assert!(header.contains("HwTypeEnum"));
        assert!(header.contains("McuTypeEnum"));
        // No double-`BLUEBERRY_` prefix.
        assert!(!header.contains("BLUEBERRY_BLUEBERRY_"));
        // Add signature matches legacy shape.
        assert!(header.contains("BbBlock addVersionMessage(Bb * buf, uint32_t firmwareVersion, uint8_t hardwareRev, HwTypeEnum hardwareType, McuTypeEnum mcuType);"));

        // Source — VERSION_MESSAGE_LENGTH and indices.
        assert!(source.contains("#define VERSION_MESSAGE_LENGTH (16)"));
        assert!(source.contains("#define VERSION_MESSAGE_FIRMWARE_VERSION_INDEX (8)"));
        assert!(source.contains("#define VERSION_MESSAGE_HARDWARE_REV_INDEX (12)"));
        // mcuType + hardwareType ordering: u8 at 13, u16 at 14 (legacy reorders for alignment)
        assert!(source.contains("#define VERSION_MESSAGE_MCU_TYPE_INDEX (13)"));
        assert!(source.contains("#define VERSION_MESSAGE_HARDWARE_TYPE_INDEX (14)"));
        // Ordinals stay in IDL order.
        assert!(source.contains("#define VERSION_MESSAGE_FIRMWARE_VERSION_ORDINAL (3)"));
        assert!(source.contains("#define VERSION_MESSAGE_HARDWARE_REV_ORDINAL (4)"));
        assert!(source.contains("#define VERSION_MESSAGE_HARDWARE_TYPE_ORDINAL (5)"));
        assert!(source.contains("#define VERSION_MESSAGE_MCU_TYPE_ORDINAL (6)"));
        assert!(source.contains("#define VERSION_MESSAGE_MAX_ORDINAL (6)"));
        assert!(source.contains("#define VERSION_MESSAGE_MODULE_MESSAGE_KEY (0x42448366)"));

        // Topic constant.
        assert!(source.contains("const char VERSION_MESSAGE_TOPIC[]"));
        assert!(source.contains("blueberry/devices/\\x81/\\x80/version"));
    }

    /// Regression: the `addVersionMessage` builder body must use the exact
    /// `setBb*` call sequence and the exact `VERSION_MESSAGE_*_INDEX` macros
    /// the legacy `blueberry-c/src/blueberry_devices.c` emits.  This guards
    /// against accidental reorderings of the wire layout that would break
    /// firmware ABI compatibility.
    #[test]
    fn version_message_add_body_matches_legacy() {
        let src = r#"
            @module_key(0x4244)
            module ::Blueberry::Devices {
                enum HwType : uint16 {
                    UNDEFINED = 0xffff
                };
                enum McuType : uint8 {
                    UNDEFINED = 0xff
                };
                @topic("blueberry/devices/{device_type}/{nid}/version")
                @message_key(0x8366)
                message VersionMessage {
                    uint32 firmwareVersion;
                    uint8 hardwareRev;
                    HwType hardwareType;
                    McuType mcuType;
                };
            };
        "#;
        let defs = parse(src);
        let files = generate(&defs).unwrap();
        let source = &files[1].contents;

        // Exact body of the legacy `addVersionMessage` builder, verbatim from
        // blueberry-vanilla-firmware/blueberry-c/src/blueberry_devices.c.
        let expected = "BbBlock addVersionMessage(Bb * buf, uint32_t firmwareVersion, uint8_t hardwareRev, HwTypeEnum hardwareType, McuTypeEnum mcuType){\n\
\tBbBlock msg = buf->length;\n\
\t//Extend buffer to include the main message body before writing it\n\
\tbuf->length = msg + VERSION_MESSAGE_LENGTH;\n\
\tsetBbUint32(buf, msg, VERSION_MESSAGE_MODULE_MESSAGE_KEY_INDEX, VERSION_MESSAGE_MODULE_MESSAGE_KEY);\n\
\tsetBbUint16(buf, msg, VERSION_MESSAGE_LENGTH_INDEX, VERSION_MESSAGE_LENGTH/4);//length field is measured in 4-byte words\n\
\tsetBbUint8(buf, msg, VERSION_MESSAGE_MAX_ORDINAL_INDEX, VERSION_MESSAGE_MAX_ORDINAL);\n\
\tsetBbUint32(buf, msg, VERSION_MESSAGE_FIRMWARE_VERSION_INDEX, firmwareVersion);\n\
\tsetBbUint8(buf, msg, VERSION_MESSAGE_HARDWARE_REV_INDEX, hardwareRev);\n\
\tsetBbUint16(buf, msg, VERSION_MESSAGE_HARDWARE_TYPE_INDEX, hardwareType);\n\
\tsetBbUint8(buf, msg, VERSION_MESSAGE_MCU_TYPE_INDEX, mcuType);\n\
\treturn msg;\n\
}";
        assert!(
            source.contains(expected),
            "addVersionMessage body diverged from legacy ABI.\nGenerated:\n{}",
            source
        );
    }

    #[test]
    fn manifest_messages_emit_new_symbols() {
        let src = r#"
            @module_key(0x4244)
            module ::Blueberry::Devices {
                @topic("blueberry/devices/{device_type}/{nid}/manifest-summary")
                @message_key(0x9100)
                message ManifestSummaryMessage {
                    uint16 schemaVersion;
                    uint32 manifestCrc;
                    uint16 totalParts;
                };
                @topic("blueberry/devices/{device_type}/{nid}/get-manifest")
                @message_key(0x9101)
                message GetManifestMessage {
                    uint32 sinceCrc;
                    uint16 fromPartIndex;
                };
            };
        "#;
        let defs = parse(src);
        let files = generate(&defs).unwrap();
        let header = &files[0].contents;
        assert!(
            header.contains("#define BLUEBERRY_DEVICES_MANIFEST_SUMMARY_MESSAGE_KEY (0x42449100)")
        );
        assert!(header.contains("#define BLUEBERRY_DEVICES_GET_MANIFEST_MESSAGE_KEY (0x42449101)"));
        // Builder prototypes
        assert!(header.contains("BbBlock addManifestSummaryMessage(Bb * buf,"));
        assert!(header.contains("BbBlock addGetManifestMessage(Bb * buf,"));
        // Field accessor for `sinceCrc`
        assert!(header.contains("uint32_t getGetManifestMessageSinceCrc(Bb * buf, BbBlock msg );"));
        assert!(
            header.contains("uint16_t getGetManifestMessageFromPartIndex(Bb * buf, BbBlock msg );")
        );
    }
}

// ---------------------------------------------------------------------------
// Suppress unused warnings for AST imports used only via destructuring.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _unused_imports(
    _td: TypeDef,
    _md: ModuleDef,
    _enum: EnumDef,
    _struct: StructDef,
    _msg: MessageDef,
    _member: Member,
) {
}

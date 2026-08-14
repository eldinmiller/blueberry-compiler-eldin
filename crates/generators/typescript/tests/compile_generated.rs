//! End-to-end compile test for the TypeScript generator.
//!
//! Parses `blueberry_full.idl`, runs the generator, drops outputs + a minimal
//! npm project (with `blueberry-serde-ts` pulled from GitHub) into a tempdir,
//! installs deps, and runs `tsc --noEmit`. Asserts exit code 0.
//!
//! Hard-fails if Node or npm are absent — matches the Rust generator's
//! hard-fail-on-missing-cargo behaviour.

use std::{fs, path::PathBuf, process::Command};

use blueberry_generator_typescript::generate;
use blueberry_parser::parse_idl;

const PACKAGE_JSON: &str = r#"{
  "name": "blueberry-generated-typescript",
  "version": "0.0.0",
  "private": true,
  "type": "module",
  "dependencies": {
    "blueberry-serde-ts": "github:eldinmiller/blueberry-serde-ts#v0.1.1"
  },
  "devDependencies": {
    "typescript": "^5.4.0"
  }
}
"#;

const TSCONFIG_JSON: &str = r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "strict": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "forceConsistentCasingInFileNames": true,
    "noUncheckedIndexedAccess": false,
    "noEmit": true,
    "resolveJsonModule": true,
    "isolatedModules": true
  },
  "include": ["typescript/**/*.ts"]
}
"#;

fn npm_command() -> Command {
    if cfg!(windows) {
        let mut cmd = Command::new("npm.cmd");
        cmd.env("NODE_NO_WARNINGS", "1");
        cmd
    } else {
        Command::new("npm")
    }
}

fn require_npm() {
    let output = npm_command()
        .arg("--version")
        .output()
        .expect("npm must be installed to run TypeScript compile tests");
    assert!(
        output.status.success(),
        "npm --version exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generated_typescript_from_blueberry_full_compiles() {
    require_npm();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .and_then(|path| path.parent())
        .expect("workspace root directory");
    let fixture_path = workspace_root.join("crates/parser/tests/fixtures/blueberry_full.idl");

    let contents = fs::read_to_string(&fixture_path).expect("read blueberry_full.idl");
    let definitions = parse_idl(&contents).expect("parse blueberry_full.idl");
    let files = generate(&definitions).expect("typescript generation should succeed");

    let temp_dir = tempfile::tempdir().expect("create temp dir");

    for file in &files {
        let path = temp_dir.path().join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create generated subdir");
        }
        fs::write(&path, &file.contents).expect("write generated typescript");
    }

    if let Ok(dir) = std::env::var("BLUEBERRY_DUMP_GENERATED") {
        for file in &files {
            let dump_path = PathBuf::from(&dir).join(file.path.clone());
            if let Some(parent) = dump_path.parent() {
                fs::create_dir_all(parent).expect("create dump directory");
            }
            fs::write(&dump_path, &file.contents).expect("dump generated typescript");
            eprintln!("Wrote generated output to {}", dump_path.display());
        }
    }

    fs::write(temp_dir.path().join("package.json"), PACKAGE_JSON).expect("write package.json");
    fs::write(temp_dir.path().join("tsconfig.json"), TSCONFIG_JSON).expect("write tsconfig.json");

    let install_output = npm_command()
        .arg("install")
        .arg("--no-audit")
        .arg("--no-fund")
        .arg("--loglevel=error")
        .current_dir(temp_dir.path())
        .output()
        .expect("npm install");
    assert!(
        install_output.status.success(),
        "npm install failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&install_output.stdout),
        String::from_utf8_lossy(&install_output.stderr),
    );

    let tsc_path = if cfg!(windows) {
        temp_dir.path().join("node_modules/.bin/tsc.cmd")
    } else {
        temp_dir.path().join("node_modules/.bin/tsc")
    };
    assert!(
        tsc_path.exists(),
        "tsc binary missing at {}",
        tsc_path.display(),
    );

    let tsc_output = Command::new(&tsc_path)
        .arg("--noEmit")
        .arg("--project")
        .arg(temp_dir.path().join("tsconfig.json"))
        .current_dir(temp_dir.path())
        .output()
        .expect("invoke tsc");
    assert!(
        tsc_output.status.success(),
        "tsc --noEmit failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&tsc_output.stdout),
        String::from_utf8_lossy(&tsc_output.stderr),
    );
}

#[test]
fn lowercase_idl_enum_emits_pascal_case_declaration() {
    let idl = r#"
        module Blueberry {
            enum parameter8 : uint8 {
                REVID = 0,
                ID = 1,
            };
            @message_key(0xc342)
            @topic("blueberry/devices/tcs3400-data8")
            message Tcs3400Data8Message {
                parameter8 param;
                uint8 data;
            };
        };
    "#;
    let definitions = parse_idl(idl).expect("parse lowercase-enum IDL");
    let files = generate(&definitions).expect("generate typescript");
    let messages = files
        .iter()
        .find(|file| file.path.ends_with("blueberry_messages.ts"))
        .expect("blueberry_messages.ts");
    assert!(
        messages.contents.contains("export enum Parameter8"),
        "expected PascalCase enum declaration, got:\n{}",
        messages.contents
    );
    assert!(
        messages.contents.contains("param: Parameter8"),
        "expected PascalCase enum type reference, got:\n{}",
        messages.contents
    );
    assert!(
        !messages.contents.contains("export enum parameter8"),
        "IDL camelCase enum name leaked into the declaration:\n{}",
        messages.contents
    );
}

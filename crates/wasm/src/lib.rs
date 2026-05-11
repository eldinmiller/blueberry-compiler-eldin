#![cfg(target_arch = "wasm32")]

use blueberry_codegen_core::{CodegenError, GeneratedFile};
use blueberry_generator_c as c_generator;
use blueberry_generator_cpp as cpp_generator;
use blueberry_generator_idl::generate_idl;
use blueberry_generator_python as python_generator;
use blueberry_generator_rust::generate_rust;
use blueberry_generator_typescript as typescript_generator;
use blueberry_parser::{Definition, parse_idl};
use js_sys::{Array, Object, Reflect};
use wasm_bindgen::{JsValue, prelude::*};

#[wasm_bindgen(start)]
pub fn wasm_start() {
    console_error_panic_hook::set_once();
}

/// Parse the given IDL source and return either the formatted AST
/// or the parse error description.
#[wasm_bindgen]
pub fn parse_idl_wasm(input: &str) -> String {
    match parse_idl(input) {
        Ok(defs) => format!("{:#?}", defs),
        Err(err) => err.to_string(),
    }
}

/// Parse and reserialize the given IDL, returning a JS object with both results.
#[wasm_bindgen]
pub fn analyze_idl_wasm(input: &str, mode: &str) -> JsValue {
    let result = Object::new();
    match parse_idl(input) {
        Ok(defs) => {
            let ast = format!("{:#?}", defs);
            let generated = match mode {
                "rust" => "Rust generation emits files; see the codegen.rust entry for results"
                    .to_string(),
                _ => generate_idl(&defs),
            };
            let codegen = build_codegen_object(&defs);
            Reflect::set(&result, &"ast".into(), &JsValue::from_str(&ast)).unwrap();
            Reflect::set(&result, &"generated".into(), &JsValue::from_str(&generated)).unwrap();
            Reflect::set(&result, &"error".into(), &JsValue::UNDEFINED).unwrap();
            Reflect::set(&result, &"codegen".into(), &codegen).unwrap();
        }
        Err(err) => {
            let err_msg = err.to_string();
            let err_str = JsValue::from_str(&err_msg);
            Reflect::set(&result, &"ast".into(), &err_str).unwrap();
            let target = match mode {
                "rust" => "Rust",
                _ => "IDL",
            };
            let generated_msg = format!("Cannot generate {target}:\n{err_msg}");
            Reflect::set(
                &result,
                &"generated".into(),
                &JsValue::from_str(&generated_msg),
            )
            .unwrap();
            Reflect::set(&result, &"error".into(), &JsValue::from_str(&err_msg)).unwrap();
            Reflect::set(&result, &"codegen".into(), &JsValue::UNDEFINED).unwrap();
        }
    }
    result.into()
}

type GeneratorFn = fn(&[Definition]) -> Result<Vec<GeneratedFile>, CodegenError>;

fn build_codegen_object(defs: &[Definition]) -> JsValue {
    const TARGETS: [(&str, GeneratorFn); 5] = [
        ("python", python_generator::generate),
        ("c", c_generator::generate),
        ("cpp", cpp_generator::generate),
        ("rust", generate_rust),
        ("typescript", typescript_generator::generate),
    ];

    let codegen = Object::new();
    for (key, generator) in TARGETS {
        let entry = Object::new();
        match generator(defs) {
            Ok(files) => {
                let files_array = files_to_js_array(&files);
                Reflect::set(&entry, &"files".into(), &files_array.into()).unwrap();
                Reflect::set(&entry, &"error".into(), &JsValue::UNDEFINED).unwrap();
            }
            Err(err) => {
                let message = describe_codegen_error(&err);
                Reflect::set(&entry, &"files".into(), &Array::new().into()).unwrap();
                Reflect::set(&entry, &"error".into(), &JsValue::from_str(&message)).unwrap();
            }
        }
        Reflect::set(&codegen, &key.into(), &entry.into()).unwrap();
    }
    codegen.into()
}

fn files_to_js_array(files: &[GeneratedFile]) -> Array {
    let array = Array::new();
    for file in files {
        let file_obj = Object::new();
        Reflect::set(&file_obj, &"path".into(), &JsValue::from_str(&file.path)).unwrap();
        Reflect::set(
            &file_obj,
            &"contents".into(),
            &JsValue::from_str(&file.contents),
        )
        .unwrap();
        array.push(&file_obj.into());
    }
    array
}

fn describe_codegen_error(err: &CodegenError) -> String {
    match err {
        CodegenError::MissingTopic { message } => {
            format!("Message \"{message}\" is missing a @topic annotation")
        }
        CodegenError::UnsupportedMessageBase { message } => {
            format!("Message \"{message}\" cannot inherit from another type for code generation")
        }
        CodegenError::UnsupportedMemberType {
            message,
            member,
            type_name,
        } => format!(
            "Message \"{message}\" member \"{member}\" uses unsupported type \"{type_name}\""
        ),
        CodegenError::DuplicateMessageKey {
            module_key,
            message_key,
            first,
            second,
        } => format!(
            "Duplicate @message_key(0x{message_key:04x}) in module 0x{module_key:04x}: used by \"{first}\" and \"{second}\""
        ),
    }
}

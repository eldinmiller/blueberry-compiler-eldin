use blueberry_ast::{
    Annotation, AnnotationParam, Commented, ConstValue, Definition, MessageDef, Type,
};

pub mod type_registry;
pub use type_registry::{ResolvedMember, TypeRegistry, map_builtin_ident};

pub const DEFAULT_MODULE_KEY: u16 = 0;
pub const MESSAGE_HEADER_SIZE: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodegenError {
    MissingTopic {
        message: String,
    },
    UnsupportedMessageBase {
        message: String,
    },
    UnsupportedMemberType {
        message: String,
        member: String,
        type_name: String,
    },
    DuplicateMessageKey {
        module_key: u16,
        message_key: u16,
        first: String,
        second: String,
    },
}

#[derive(Debug, Clone)]
pub struct MessageSpec {
    pub scope: Vec<String>,
    pub name: String,
    pub topic: String,
    pub module_key: u16,
    pub message_key: u16,
    pub fields: Vec<FieldSpec>,
}

impl MessageSpec {
    pub fn field_payload_size(&self) -> usize {
        self.fields.iter().map(|field| field.primitive.size()).sum()
    }

    pub fn padded_payload_size(&self) -> usize {
        let size = self.field_payload_size();
        size.div_ceil(4) * 4
    }

    pub fn payload_words(&self) -> usize {
        let padded = self.padded_payload_size();
        if padded == 0 { 0 } else { padded / 4 }
    }
}

#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub name: String,
    pub primitive: PrimitiveType,
}

#[derive(Copy, Clone, Debug)]
pub enum PrimitiveType {
    Bool,
    Char,
    Octet,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}

impl PrimitiveType {
    pub fn size(self) -> usize {
        match self {
            PrimitiveType::Bool | PrimitiveType::Char | PrimitiveType::Octet => 1,
            PrimitiveType::I16 | PrimitiveType::U16 => 2,
            PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::F32 => 4,
            PrimitiveType::I64 | PrimitiveType::U64 | PrimitiveType::F64 => 8,
        }
    }
}

pub fn collect_messages(defs: &[Definition]) -> Result<Vec<MessageSpec>, CodegenError> {
    let mut collector = MessageCollector::new();
    collector.collect(defs)?;
    Ok(collector.messages)
}

struct MessageCollector {
    messages: Vec<MessageSpec>,
    message_key: u16,
}

impl MessageCollector {
    fn new() -> Self {
        Self {
            messages: Vec::new(),
            message_key: 1,
        }
    }

    fn next_message_key(&mut self) -> u16 {
        let key = self.message_key;
        self.message_key = self.message_key.wrapping_add(1);
        if self.message_key == 0 {
            self.message_key = 1;
        }
        key
    }

    fn collect(&mut self, defs: &[Definition]) -> Result<(), CodegenError> {
        let mut scope = Vec::new();
        self.collect_in_scope(defs, &mut scope, DEFAULT_MODULE_KEY)
    }

    fn collect_in_scope(
        &mut self,
        defs: &[Definition],
        scope: &mut Vec<String>,
        module_key: u16,
    ) -> Result<(), CodegenError> {
        for def in defs {
            match def {
                Definition::ModuleDef(module_def) => {
                    scope.push(module_def.node.name.clone());
                    let module_key =
                        annotation_u16(&module_def.annotations, "module_key").unwrap_or(module_key);
                    self.collect_in_scope(&module_def.node.definitions, scope, module_key)?;
                    scope.pop();
                }
                Definition::MessageDef(message_def) => {
                    let spec = self.visit_message(scope, module_key, message_def)?;
                    self.messages.push(spec);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn visit_message(
        &mut self,
        scope: &[String],
        module_key: u16,
        message_def: &Commented<MessageDef>,
    ) -> Result<MessageSpec, CodegenError> {
        if message_def.node.base.is_some() {
            return Err(CodegenError::UnsupportedMessageBase {
                message: scoped_name(scope, &message_def.node.name),
            });
        }
        let topic = annotation_string(&message_def.annotations, "topic").ok_or(
            CodegenError::MissingTopic {
                message: scoped_name(scope, &message_def.node.name),
            },
        )?;
        let module_key =
            annotation_u16(&message_def.annotations, "module_key").unwrap_or(module_key);
        let message_key = annotation_u16(&message_def.annotations, "message_key")
            .unwrap_or_else(|| self.next_message_key());
        let mut fields = Vec::new();
        for member in &message_def.node.members {
            let primitive = match PrimitiveType::try_from(&member.node.type_) {
                Ok(primitive) => primitive,
                Err(_) => {
                    let type_name = match &member.node.type_ {
                        Type::Float => "float",
                        Type::Double => "double",
                        Type::LongDouble => "long double",
                        Type::Long => "long",
                        Type::UnsignedLong => "unsigned long",
                        Type::Short => "short",
                        Type::UnsignedShort => "unsigned short",
                        Type::LongLong => "long long",
                        Type::UnsignedLongLong => "unsigned long long",
                        Type::Octet => "octet",
                        Type::Boolean => "boolean",
                        Type::Char => "char",
                        Type::WChar => "wchar",
                        Type::String { .. } => "string",
                        Type::WString => "wstring",
                        Type::Sequence { .. } => "sequence",
                        Type::Array { .. } => "array",
                        Type::ScopedName(path) => {
                            return Err(CodegenError::UnsupportedMemberType {
                                message: scoped_name(scope, &message_def.node.name),
                                member: member.node.name.clone(),
                                type_name: path.join("::"),
                            });
                        }
                    }
                    .to_string();

                    return Err(CodegenError::UnsupportedMemberType {
                        message: scoped_name(scope, &message_def.node.name),
                        member: member.node.name.clone(),
                        type_name,
                    });
                }
            };
            fields.push(FieldSpec {
                name: member.node.name.clone(),
                primitive,
            });
        }
        Ok(MessageSpec {
            scope: scope.to_vec(),
            name: message_def.node.name.clone(),
            topic,
            module_key,
            message_key,
            fields,
        })
    }
}

impl TryFrom<&Type> for PrimitiveType {
    type Error = ();

    fn try_from(value: &Type) -> Result<Self, Self::Error> {
        match value {
            Type::Float => Ok(PrimitiveType::F32),
            Type::Double => Ok(PrimitiveType::F64),
            Type::Long => Ok(PrimitiveType::I32),
            Type::UnsignedLong => Ok(PrimitiveType::U32),
            Type::Short => Ok(PrimitiveType::I16),
            Type::UnsignedShort => Ok(PrimitiveType::U16),
            Type::LongLong => Ok(PrimitiveType::I64),
            Type::UnsignedLongLong => Ok(PrimitiveType::U64),
            Type::Octet => Ok(PrimitiveType::Octet),
            Type::Boolean => Ok(PrimitiveType::Bool),
            Type::Char => Ok(PrimitiveType::Char),
            _ => Err(()),
        }
    }
}

fn annotation_string(annotations: &[Annotation], name: &str) -> Option<String> {
    annotation_value(annotations, name).and_then(|value| match value {
        ConstValue::String(value) => Some(value.clone()),
        _ => None,
    })
}

fn annotation_u16(annotations: &[Annotation], name: &str) -> Option<u16> {
    annotation_value(annotations, name).and_then(|value| match value {
        ConstValue::Integer(lit) if (0..=u16::MAX as i128).contains(&lit.value) => {
            Some(lit.value as u16)
        }
        _ => None,
    })
}

fn annotation_value<'a>(annotations: &'a [Annotation], name: &str) -> Option<&'a ConstValue> {
    let annotation = annotations
        .iter()
        .find(|annotation| annotation_name_matches(annotation, name))?;

    annotation
        .params
        .iter()
        .map(|param| match param {
            AnnotationParam::Named { name, value } if name.eq_ignore_ascii_case("value") => value,
            AnnotationParam::Positional(value) => value,
            AnnotationParam::Named { value, .. } => value,
        })
        .next()
}

fn annotation_name_matches(annotation: &Annotation, expected: &str) -> bool {
    annotation
        .name
        .last()
        .map(|segment| segment.eq_ignore_ascii_case(expected))
        .unwrap_or(false)
}

pub fn scoped_name(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", scope.join("::"), name)
    }
}

pub fn to_snake_case(identifier: &str) -> String {
    let mut result = String::new();
    let mut prev_lower = false;
    for ch in identifier.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower && !result.ends_with('_') {
                result.push('_');
            }
            result.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else if ch == ' ' || ch == '-' {
            if !result.ends_with('_') {
                result.push('_');
            }
            prev_lower = false;
        } else {
            result.push(ch.to_ascii_lowercase());
            prev_lower = ch.is_ascii_alphanumeric() && ch.is_ascii_lowercase();
        }
    }
    result
}

pub fn snake_case_path(scope: &[String], name: &str) -> String {
    let mut parts: Vec<String> = scope.iter().map(|s| to_snake_case(s)).collect();
    parts.push(to_snake_case(name));
    parts.join("_")
}

pub fn class_name(scope: &[String], name: &str) -> String {
    let mut parts = scope.to_vec();
    parts.push(name.to_string());
    parts.join("_")
}

pub fn uppercase_path(scope: &[String], name: &str) -> String {
    snake_case_path(scope, name).to_ascii_uppercase()
}

pub fn quoted_string(value: &str) -> String {
    // Produces a double-quoted, escaped literal suitable for generators that embed raw text
    // (C, C++, Python, etc.) without relying on proc-macro tokenization.
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    format!("\"{escaped}\"")
}

pub fn topic_format(template: &str) -> TopicFormat {
    let mut fmt = String::new();
    let mut placeholders = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut name = String::new();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == '}' {
                    break;
                }
                name.push(next);
            }
            match name.as_str() {
                "device_type" => {
                    fmt.push_str("%s");
                    placeholders.push(TopicPlaceholder::DeviceType);
                }
                "nid" => {
                    fmt.push_str("%s");
                    placeholders.push(TopicPlaceholder::Nid);
                }
                _ => {
                    fmt.push('{');
                    fmt.push_str(&name);
                    fmt.push('}');
                }
            }
        } else {
            fmt.push(ch);
        }
    }
    TopicFormat {
        template: fmt,
        placeholders,
    }
}

#[derive(Debug, Clone)]
pub struct TopicFormat {
    pub template: String,
    pub placeholders: Vec<TopicPlaceholder>,
}

#[derive(Debug, Clone, Copy)]
pub enum TopicPlaceholder {
    DeviceType,
    Nid,
}

//! Emit `export enum` declarations with explicit discriminants.

use blueberry_ast::{Commented, ConstValue, EnumDef};

use crate::TypeScriptGenerator;
use crate::naming::pascal_case;

impl TypeScriptGenerator {
    pub(crate) fn emit_enum(&self, enum_def: &Commented<EnumDef>) -> String {
        // Match render_ts_type / decode casts, which PascalCase scoped names.
        let name = pascal_case(&enum_def.node.name);
        let mut out = String::new();
        out.push_str(&format!("export enum {name} {{\n"));

        let mut next_implicit: i128 = 0;
        for member in &enum_def.node.enumerators {
            let value = match &member.value {
                Some(ConstValue::Integer(lit)) => lit.value,
                Some(ConstValue::Binary(bin)) => bin.to_i128(),
                _ => next_implicit,
            };
            next_implicit = value.wrapping_add(1);
            out.push_str(&format!("  {} = {},\n", member.name, value));
        }

        out.push_str("}\n");
        out
    }
}

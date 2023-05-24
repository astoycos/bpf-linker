use super::dw_tag::dw_tag_from_value_str;
use super::message::Message;
use super::symbol_name;
use crate::llvm::iter::*;
use crate::llvm::to_string;
use gimli::constants::DwTag;
use gimli::DW_TAG_pointer_type;
use gimli::DW_TAG_structure_type;
use gimli::DW_TAG_variant_part;
use llvm_sys::core::*;
use llvm_sys::debuginfo::*;
use llvm_sys::prelude::*;
use log::*;
use std::collections::HashSet;
use std::ffi::CStr;

pub struct DIFix {
    context: LLVMContextRef,
    module: LLVMModuleRef,
    builder: LLVMDIBuilderRef,
    cache: Cache,
    node_stack: Vec<LLVMValueRef>,
}

// Sanitize Rust type names to be valid C type names.
fn sanitize_type_name(name: String) -> String {
    name.chars()
        .map(|ch| {
            // Characters which are valid in C type names (alphanumeric and `_`).
            if matches!(ch, '0'..='9' | 'A'..='Z' | 'a'..='z' | '_') {
                ch.to_string()
            } else {
                format!("_{:X}_", ch as u32)
            }
        })
        .collect()
}

impl DIFix {
    pub unsafe fn new(context: LLVMContextRef, module: LLVMModuleRef) -> DIFix {
        DIFix {
            context,
            module,
            builder: LLVMCreateDIBuilder(module),
            cache: Cache::new(),
            node_stack: Vec::new(),
        }
    }

    unsafe fn mdnode(&mut self, value: LLVMValueRef) {
        let metadata = LLVMValueAsMetadata(value);
        let metadata_kind = LLVMGetMetadataKind(metadata);

        let empty = to_mdstring(self.context, "");

        match metadata_kind {
            LLVMMetadataKind::LLVMDICompositeTypeMetadataKind => {
                let tag = get_tag(value);

                #[allow(non_upper_case_globals)]
                match tag {
                    Some(DW_TAG_structure_type) => {
                        let mut len = 0;
                        let name =
                            to_string(LLVMDITypeGetName(LLVMValueAsMetadata(value), &mut len));

                        if name.starts_with("HashMap<") {
                            // Remove name from BTF map structs.
                            LLVMReplaceMDNodeOperandWith(value, 2, empty);
                        } else {
                            // Clear the name from generics.
                            let name = sanitize_type_name(name);
                            let name = to_mdstring(self.context, &name);
                            LLVMReplaceMDNodeOperandWith(value, 2, name);
                        }

                        // variadic enum not supported => emit warning and strip out the children array
                        // i.e. pub enum Foo { Bar, Baz(u32), Bad(u64, u64) }

                        // we detect this is a variadic enum if the child element is a DW_TAG_variant_part
                        let elements = LLVMGetOperand(value, 4);
                        let num_elements = LLVMGetNumOperands(elements);
                        if num_elements > 0 {
                            let element = LLVMGetOperand(elements, 0);
                            if get_tag(element) == Some(DW_TAG_variant_part) {
                                let link = "http://none-yet";

                                let mut len = 0;
                                let name = to_string(LLVMDITypeGetName(
                                    LLVMValueAsMetadata(value),
                                    &mut len,
                                ));

                                // TODO: check: the following always returns <unknown>:0 - however its strange...
                                let mut _len = 0;
                                let _line = LLVMDITypeGetLine(LLVMValueAsMetadata(value)); // always returns 0
                                let scope = LLVMDIVariableGetScope(metadata);
                                let file = LLVMDIScopeGetFile(scope);
                                let mut len = 0;
                                let _filename = to_string(LLVMDIFileGetFilename(file, &mut len)); // still getting <undefined>

                                // FIX: shadowing prev values with "correct" ones, found looking at parent nodes
                                let (filename, line) = self
                                    .node_stack
                                    .iter()
                                    .rev()
                                    .find_map(|v: &LLVMValueRef| -> Option<(String, u32)> {
                                        let v = *v;
                                        if !is_mdnode(v) {
                                            return None;
                                        }
                                        let m = LLVMValueAsMetadata(v);
                                        let metadata_kind = LLVMGetMetadataKind(m);
                                        let file_operand_index = match metadata_kind {
                                            LLVMMetadataKind::LLVMDIGlobalVariableMetadataKind => {
                                                Some(2)
                                            }
                                            LLVMMetadataKind::LLVMDICommonBlockMetadataKind => {
                                                Some(3)
                                            }
                                            // TODO: add more cases based on asmwriter.cpp
                                            _ => None,
                                        }?;
                                        let file = LLVMGetOperand(v, file_operand_index);
                                        let mut len = 0;
                                        let filename = to_string(LLVMDIFileGetFilename(
                                            LLVMValueAsMetadata(file),
                                            &mut len,
                                        ));
                                        if filename == "<unknown>" {
                                            return None;
                                        }
                                        // since this node has plausible filename, we also trust the corresponding line
                                        let line = LLVMDITypeGetLine(m);
                                        Some((filename, line))
                                    })
                                    .unwrap_or(("unknown".to_owned(), 0));

                                // finally emit warning
                                warn!(
                                    "at {}:{}: enum {}: not emitting BTF for type - see {}",
                                    filename, line, name, link
                                );

                                // strip out children
                                let empty_node =
                                    LLVMMDNodeInContext2(self.context, core::ptr::null_mut(), 0);
                                LLVMReplaceMDNodeOperandWith(value, 4, empty_node);

                                // remove rust names
                                LLVMReplaceMDNodeOperandWith(value, 2, empty);
                            }
                        }
                    }
                    _ => (),
                }
            }
            LLVMMetadataKind::LLVMDIDerivedTypeMetadataKind => {
                let tag = get_tag(value);

                #[allow(non_upper_case_globals)]
                match tag {
                    Some(DW_TAG_pointer_type) => {
                        // remove rust names
                        LLVMReplaceMDNodeOperandWith(value, 2, empty);
                    }
                    _ => (),
                }
            }
            // Sanitize function (subprogram) names.
            LLVMMetadataKind::LLVMDISubprogramMetadataKind => {
                let mut len = 0;
                let name = to_string(LLVMDITypeGetName(LLVMValueAsMetadata(value), &mut len));

                // Clear the name from generics.
                let name = sanitize_type_name(name);
                let name = to_mdstring(self.context, &name);
                LLVMReplaceMDNodeOperandWith(value, 2, name);
            }
            _ => (),
        }
    }

    // navigate the tree of LLVMValueRefs (DFS-pre-order)
    unsafe fn discover(&mut self, value: LLVMValueRef, depth: usize) {
        let indent = indent(depth);

        if value.is_null() {
            trace!("{}skipping null node", indent);
            return;
        }

        // TODO: doing this on the pointer value is not good
        let key = if is_mdnode(value) {
            LLVMValueAsMetadata(value) as u64
        } else {
            value as u64
        };
        if self.cache.hit(&key) {
            trace!("{}skipping already visited node", indent);
            return;
        }

        self.node_stack.push(value);

        if is_mdnode(value) {
            let metadata = LLVMValueAsMetadata(value);
            let metadata_kind = LLVMGetMetadataKind(metadata);

            trace!(
                "{}mdnode kind:{:?} n_operands:{} value: {}",
                indent,
                metadata_kind,
                LLVMGetMDNodeNumOperands(value),
                Message::from_ptr(LLVMPrintValueToString(value))
                    .to_str()
                    .unwrap_or("")
            );

            self.mdnode(value)
        } else {
            trace!(
                "{}node value: {}",
                indent,
                Message::from_ptr(LLVMPrintValueToString(value))
                    .to_str()
                    .unwrap_or("")
            );
        }

        if can_get_all_metadata(value) {
            for (index, (kind, metadata)) in iter_medatada_copy(value).enumerate() {
                let metadata_value = LLVMMetadataAsValue(self.context, metadata);
                trace!("{}all_metadata entry: index:{}", indent, index);
                self.discover(metadata_value, depth + 1);

                if is_instruction(value) {
                    LLVMSetMetadata(value, kind, metadata_value);
                } else {
                    LLVMGlobalSetMetadata(value, kind, metadata);
                }
            }
        }

        if can_get_operands(value) {
            for (index, operand) in iter_operands(value).enumerate() {
                trace!(
                    "{}operand index:{} name:{} value:{}",
                    indent,
                    index,
                    symbol_name(value),
                    Message::from_ptr(LLVMPrintValueToString(operand))
                        .to_str()
                        .unwrap_or("")
                );
                self.discover(operand, depth + 1)
            }
        }

        self.node_stack.pop();
    }

    pub unsafe fn run(&mut self) {
        for sym in self.module.named_metadata_iter() {
            let mut len: usize = 0;
            let name = CStr::from_ptr(LLVMGetNamedMetadataName(sym, &mut len))
                .to_str()
                .unwrap_or("");
            // just for debugging, we are not visiting those nodes for the moment
            trace!("named metadata name:{}", name);
        }

        let module = self.module;
        for (i, sym) in module.globals_iter().enumerate() {
            trace!("global index:{} name:{}", i, symbol_name(sym));
            self.discover(sym, 0);
        }

        for (i, sym) in module.global_aliases_iter().enumerate() {
            trace!("global aliases index:{} name:{}", i, symbol_name(sym));
            self.discover(sym, 0);
        }

        for function in module.functions_iter() {
            trace!("function > name:{}", symbol_name(function));
            self.discover(function, 0);

            let params_count = LLVMCountParams(function);
            for i in 0..params_count {
                let param = LLVMGetParam(function, i);
                trace!("function param name:{} index:{}", symbol_name(param), i);
                self.discover(param, 1);
            }

            for basic_block in function.basic_blocks_iter() {
                trace!("function block");
                for instruction in basic_block.instructions_iter() {
                    let n_operands = LLVMGetNumOperands(instruction);
                    trace!("function block instruction num_operands: {}", n_operands);
                    for index in 0..n_operands {
                        let operand = LLVMGetOperand(instruction, index as u32);
                        if is_instruction(operand) {
                            self.discover(operand, 2);
                        }
                    }

                    self.discover(instruction, 1);
                }
            }
        }

        LLVMDisposeDIBuilder(self.builder);
    }
}

// utils

unsafe fn to_mdstring(context: LLVMContextRef, s: &str) -> LLVMMetadataRef {
    let len = s.len();
    let ptr = s.as_ptr() as *const i8;
    LLVMMDStringInContext2(context, ptr, len)
}

unsafe fn iter_operands(v: LLVMValueRef) -> impl Iterator<Item = LLVMValueRef> {
    (0..LLVMGetNumOperands(v)).map(move |i| LLVMGetOperand(v, i as u32))
}

unsafe fn iter_medatada_copy(v: LLVMValueRef) -> impl Iterator<Item = (u32, LLVMMetadataRef)> {
    let mut count = 0;
    let entries = LLVMGlobalCopyAllMetadata(v, &mut count);
    (0..count).map(move |index| {
        (
            LLVMValueMetadataEntriesGetKind(entries, index as u32),
            LLVMValueMetadataEntriesGetMetadata(entries, index as u32),
        )
    })
}

unsafe fn is_instruction(v: LLVMValueRef) -> bool {
    !LLVMIsAInstruction(v).is_null()
}

unsafe fn is_mdnode(v: LLVMValueRef) -> bool {
    !LLVMIsAMDNode(v).is_null()
}

unsafe fn is_user(v: LLVMValueRef) -> bool {
    !LLVMIsAUser(v).is_null()
}

unsafe fn is_globalobject(v: LLVMValueRef) -> bool {
    !LLVMIsAGlobalObject(v).is_null()
}

unsafe fn _is_globalvariable(v: LLVMValueRef) -> bool {
    !LLVMIsAGlobalVariable(v).is_null()
}

unsafe fn _is_function(v: LLVMValueRef) -> bool {
    !LLVMIsAFunction(v).is_null()
}

unsafe fn can_get_all_metadata(v: LLVMValueRef) -> bool {
    is_globalobject(v) || is_instruction(v)
}

unsafe fn can_get_operands(v: LLVMValueRef) -> bool {
    is_mdnode(v) || is_user(v)
}

unsafe fn get_tag(v: LLVMValueRef) -> Option<DwTag> {
    let msg = Message::from_ptr(LLVMPrintValueToString(v));
    let value_as_string = msg.to_str().unwrap_or("");
    let tag = dw_tag_from_value_str(value_as_string);
    tag
}

fn indent(depth: usize) -> String {
    (0..depth).map(|_| "    ").collect::<Vec<&str>>().join("")
}

pub struct Cache {
    keys: HashSet<u64>,
}

impl Cache {
    pub fn new() -> Self {
        Cache {
            keys: HashSet::new(),
        }
    }

    pub fn hit(&mut self, key: &u64) -> bool {
        if self.keys.contains(key) {
            return true;
        }
        self.keys.insert(key.clone());
        false
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_strip_generics() {
        let name = "MyStruct<u64>".to_owned();
        assert_eq!(sanitize_type_name(name), "MyStruct_3C_u64_3E_");

        let name = "MyStruct<u64, u64>".to_owned();
        assert_eq!(sanitize_type_name(name), "MyStruct_3C_u64_2C__20_u64_3E_");

        let name = "my_function<aya_bpf::BpfContext>".to_owned();
        assert_eq!(
            sanitize_type_name(name),
            "my_function_3C_aya_bpf_3A__3A_BpfContext_3E_"
        );

        let name = "my_function<aya_bpf::BpfContext, aya_log_ebpf::WriteToBuf>".to_owned();
        assert_eq!(
            sanitize_type_name(name),
            "my_function_3C_aya_bpf_3A__3A_BpfContext_2C__20_aya_log_ebpf_3A__3A_WriteToBuf_3E_"
        );

        let name = "PerfEventArray<[u8; 32]>".to_owned();
        assert_eq!(
            sanitize_type_name(name),
            "PerfEventArray_3C__5B_u8_3B__20_32_5D__3E_"
        )
    }
}
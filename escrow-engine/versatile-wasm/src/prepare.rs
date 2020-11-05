// Copyright 2018-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <http://www.gnu.org/licenses/>.

//! This module takes care of loading, checking and preprocessing of a
//! wasm module before execution. It also extracts some essential information
//! from a module.

use crate::env_def::ImportSatisfyCheck;
use crate::PrefabWasmModule;

use parity_wasm::elements::{self, External, Internal, MemoryType, Type, ValueType};
use pwasm_utils;
use pwasm_utils::rules;
use sp_runtime::traits::SaturatedConversion;
use sp_std::prelude::*;

/// Currently, all imported functions must be located inside this module. We might support
/// additional modules for versioning later.
pub const IMPORT_MODULE_FN: &str = "seal0";

/// Imported memory must be located inside this module. The reason for that is that current
/// compiler toolchains might not support specifying other modules than "env" for memory imports.
pub const IMPORT_MODULE_MEMORY: &str = "env";

// ToDo: Convert consts to parameters.
pub const VERSION: u32 = 0;
pub const REGULAR_OP_COST: u32 = 500_000;
pub const MEMORY_GROWTH_COST: u32 = REGULAR_OP_COST;
pub const MAX_TABLE_SIZE: u32 = 16 * 1024;
pub const MAX_EVENT_TOPICS: u32 = 4;
pub const MAX_MEMORY_PAGES: u32 = 16;
pub const MAX_SUBJECT_LEN: u32 = 128;
pub const MAX_STACK_HEIGHT: u32 = 64 * 1024;
pub const MAX_CODE_SIZE: u32 = 512 * 1024;
pub const ENABLE_PRINTLN: bool = false;

struct ContractModule {
    /// A deserialized module. The module is valid (this is Guaranteed by `new` method).
    module: elements::Module,
}

impl ContractModule {
    /// Creates a new instance of `ContractModule`.
    ///
    /// Returns `Err` if the `original_code` couldn't be decoded or
    /// if it contains an invalid module.
    fn new(original_code: &[u8]) -> Result<Self, &'static str> {
        use wasmi_validation::{validate_module, PlainValidator};

        let module =
            elements::deserialize_buffer(original_code).map_err(|_| "Can't decode wasm code")?;

        // Make sure that the module is valid.
        validate_module::<PlainValidator>(&module).map_err(|_| "Module is not valid")?;

        // Return a `ContractModule` instance with
        // __valid__ module.
        Ok(ContractModule { module })
    }

    /// Ensures that module doesn't declare internal memories.
    ///
    /// In this runtime we only allow wasm module to import memory from the environment.
    /// Memory section contains declarations of internal linear memories, so if we find one
    /// we reject such a module.
    fn ensure_no_internal_memory(&self) -> Result<(), &'static str> {
        if self
            .module
            .memory_section()
            .map_or(false, |ms| ms.entries().len() > 0)
        {
            return Err("module declares internal memory");
        }
        Ok(())
    }

    /// Ensures that tables declared in the module are not too big.
    fn ensure_table_size_limit(&self, limit: u32) -> Result<(), &'static str> {
        if let Some(table_section) = self.module.table_section() {
            // In Wasm MVP spec, there may be at most one table declared. Double check this
            // explicitly just in case the Wasm version changes.
            if table_section.entries().len() > 1 {
                return Err("multiple tables declared");
            }
            if let Some(table_type) = table_section.entries().first() {
                // Check the table's initial size as there is no instruction or environment function
                // capable of growing the table.
                if table_type.limits().initial() > limit {
                    return Err("table exceeds maximum size allowed");
                }
            }
        }
        Ok(())
    }

    /// Ensures that no floating point types are in use.
    fn ensure_no_floating_types(&self) -> Result<(), &'static str> {
        if let Some(global_section) = self.module.global_section() {
            for global in global_section.entries() {
                match global.global_type().content_type() {
                    ValueType::F32 | ValueType::F64 => {
                        return Err("use of floating point type in globals is forbidden");
                    }
                    _ => {}
                }
            }
        }

        if let Some(code_section) = self.module.code_section() {
            for func_body in code_section.bodies() {
                for local in func_body.locals() {
                    match local.value_type() {
                        ValueType::F32 | ValueType::F64 => {
                            return Err("use of floating point type in locals is forbidden");
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some(type_section) = self.module.type_section() {
            for wasm_type in type_section.types() {
                match wasm_type {
                    Type::Function(func_type) => {
                        let return_type = func_type.return_type();
                        for value_type in func_type.params().iter().chain(return_type.iter()) {
                            match value_type {
                                ValueType::F32 | ValueType::F64 => {
                                    return Err(
                                        "use of floating point type in function types is forbidden",
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn inject_gas_metering(self) -> Result<Self, &'static str> {
        let gas_rules = rules::Set::new(REGULAR_OP_COST.saturated_into(), Default::default())
            .with_grow_cost(MEMORY_GROWTH_COST.saturated_into())
            .with_forbidden_floats();

        let contract_module =
            pwasm_utils::inject_gas_counter(self.module, &gas_rules, IMPORT_MODULE_FN)
                .map_err(|_| "gas instrumentation failed")?;
        Ok(ContractModule {
            module: contract_module,
        })
    }

    fn inject_stack_height_metering(self) -> Result<Self, &'static str> {
        let contract_module =
            pwasm_utils::stack_height::inject_limiter(self.module, MAX_STACK_HEIGHT)
                .map_err(|_| "stack height instrumentation failed")?;
        Ok(ContractModule {
            module: contract_module,
        })
    }

    /// Check that the module has required exported functions. For now
    /// these are just entrypoints:
    ///
    /// - 'call'
    /// - 'deploy'
    ///
    /// Any other exports are not allowed.
    fn scan_exports(&self) -> Result<(), &'static str> {
        let mut deploy_found = false;
        let mut call_found = false;

        let module = &self.module;

        let types = module.type_section().map(|ts| ts.types()).unwrap_or(&[]);
        let export_entries = module
            .export_section()
            .map(|is| is.entries())
            .unwrap_or(&[]);
        let func_entries = module
            .function_section()
            .map(|fs| fs.entries())
            .unwrap_or(&[]);

        // Function index space consists of imported function following by
        // declared functions. Calculate the total number of imported functions so
        // we can use it to convert indexes from function space to declared function space.
        let fn_space_offset = module
            .import_section()
            .map(|is| is.entries())
            .unwrap_or(&[])
            .iter()
            .filter(|entry| match *entry.external() {
                External::Function(_) => true,
                _ => false,
            })
            .count();

        for export in export_entries {
            match export.field() {
                "call" => call_found = true,
                "deploy" => deploy_found = true,
                _ => return Err("unknown export: expecting only deploy and call functions"),
            }

            // Then check the export kind. "call" and "deploy" are
            // functions.
            let fn_idx = match export.internal() {
                Internal::Function(ref fn_idx) => *fn_idx,
                _ => return Err("expected a function"),
            };

            // convert index from function index space to declared index space.
            let fn_idx = match fn_idx.checked_sub(fn_space_offset as u32) {
                Some(fn_idx) => fn_idx,
                None => {
                    // Underflow here means fn_idx points to imported function which we don't allow!
                    return Err("entry point points to an imported function");
                }
            };

            // Then check the signature.
            // Both "call" and "deploy" has a () -> () function type.
            let func_ty_idx = func_entries
                .get(fn_idx as usize)
                .ok_or_else(|| "export refers to non-existent function")?
                .type_ref();
            let Type::Function(ref func_ty) = types
                .get(func_ty_idx as usize)
                .ok_or_else(|| "function has a non-existent type")?;
            if !func_ty.params().is_empty()
                || !(func_ty.return_type().is_none()
                    || func_ty.return_type() == Some(ValueType::I32))
            {
                return Err("entry point has wrong signature");
            }
        }

        if !deploy_found {
            return Err("deploy function isn't exported");
        }
        if !call_found {
            return Err("call function isn't exported");
        }

        Ok(())
    }

    // Scan an import section if any.
    //
    // This accomplishes two tasks:
    //
    // - checks any imported function against defined host functions set, incl.
    //   their signatures.
    // - if there is a memory import, returns it's descriptor
    fn scan_imports<C: ImportSatisfyCheck>(&self) -> Result<Option<&MemoryType>, &'static str> {
        let module = &self.module;

        let types = module.type_section().map(|ts| ts.types()).unwrap_or(&[]);
        let import_entries = module
            .import_section()
            .map(|is| is.entries())
            .unwrap_or(&[]);

        let mut imported_mem_type = None;

        for import in import_entries {
            let type_idx = match import.external() {
                &External::Table(_) => return Err("Cannot import tables"),
                &External::Global(_) => return Err("Cannot import globals"),
                &External::Function(ref type_idx) => {
                    if import.module() != IMPORT_MODULE_FN {
                        return Err("Invalid module for imported function");
                    }
                    type_idx
                }
                &External::Memory(ref memory_type) => {
                    if import.module() != IMPORT_MODULE_MEMORY {
                        return Err("Invalid module for imported memory");
                    }
                    if import.field() != "memory" {
                        return Err("Memory import must have the field name 'memory'");
                    }
                    if imported_mem_type.is_some() {
                        return Err("Multiple memory imports defined");
                    }
                    imported_mem_type = Some(memory_type);
                    continue;
                }
            };

            let Type::Function(ref func_ty) = types
                .get(*type_idx as usize)
                .ok_or_else(|| "validation: import entry points to a non-existent type")?;

            // We disallow importing `seal_println` unless debug features are enabled,
            // which should only be allowed on a dev chain
            if !ENABLE_PRINTLN && import.field().as_bytes() == b"seal_println" {
                return Err("module imports `seal_println` but debug features disabled");
            }

            // We disallow importing `gas` function here since it is treated as implementation detail.
            if import.field().as_bytes() == b"gas"
                || !C::can_satisfy(import.field().as_bytes(), func_ty)
            {
                println!(
                    "module imports a non-existent function {:?}",
                    import.field()
                );
                return Err("module imports a non-existent function");
            }
        }
        Ok(imported_mem_type)
    }

    fn into_wasm_code(self) -> Result<Vec<u8>, &'static str> {
        elements::serialize(self.module).map_err(|_| "error serializing instrumented module")
    }
}

/// Loads the given module given in `original_code`, performs some checks on it and
/// does some preprocessing.
///
/// The checks are:
///
/// - provided code is a valid wasm module.
/// - the module doesn't define an internal memory instance,
/// - imported memory (if any) doesn't reserve more memory than permitted by the `schedule`,
/// - all imported functions from the external environment matches defined by `env` module,
///
/// The preprocessing includes injecting code for gas metering and metering the height of stack.
pub fn prepare_contract<C: ImportSatisfyCheck>(
    original_code: &[u8],
) -> Result<PrefabWasmModule, &'static str> {
    let mut contract_module = ContractModule::new(original_code)?;
    contract_module.scan_exports()?;
    contract_module.ensure_no_internal_memory()?;
    contract_module.ensure_table_size_limit(MAX_TABLE_SIZE)?;
    contract_module.ensure_no_floating_types()?;

    struct MemoryDefinition {
        initial: u32,
        maximum: u32,
    }

    let memory_def = if let Some(memory_type) = contract_module.scan_imports::<C>()? {
        // Inspect the module to extract the initial and maximum page count.
        let limits = memory_type.limits();
        match (limits.initial(), limits.maximum()) {
            (initial, Some(maximum)) if initial > maximum => {
                return Err(
                    "Requested initial number of pages should not exceed the requested maximum",
                );
            }
            (_, Some(maximum)) if maximum > MAX_MEMORY_PAGES => {
                return Err("Maximum number of pages should not exceed the configured maximum.");
            }
            (initial, Some(maximum)) => MemoryDefinition { initial, maximum },
            (_, None) => {
                // Maximum number of pages should be always declared.
                // This isn't a hard requirement and can be treated as a maximum set
                // to configured maximum.
                return Err("Maximum number of pages should be always declared.");
            }
        }
    } else {
        // If none memory imported then just crate an empty placeholder.
        // Any access to it will lead to out of bounds trap.
        MemoryDefinition {
            initial: 0,
            maximum: 0,
        }
    };

    contract_module = contract_module
        .inject_gas_metering()?
        .inject_stack_height_metering()?;

    Ok(PrefabWasmModule {
        schedule_version: VERSION,
        initial: memory_def.initial,
        maximum: memory_def.maximum,
        _reserved: None,
        code: contract_module.into_wasm_code()?,
    })
}

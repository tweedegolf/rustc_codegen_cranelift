use std::collections::HashMap;
use std::sync::Arc;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::incremental_cache::CacheKeyHash;
use cranelift_codegen::ir::Signature;
use cranelift_codegen::isa::{TargetFrontendConfig, TargetIsa};
use cranelift_codegen::{CompiledCode, CompiledCodeStencil, Context};
use cranelift_module::{
    DataDescription, DataId, FuncId, FuncOrDataId, Linkage, Module, ModuleDeclarations,
    ModuleReloc, ModuleResult,
};
use cranelift_object::{ObjectModule, ObjectProduct};
use rustc_data_structures::sync::{IntoDynSyncSend, RwLock};

use crate::UnwindContext;

/// A wrapper around a [Module] which adds any defined function to the [UnwindContext].
pub(crate) struct UnwindModule<T> {
    pub(crate) module: T,
    unwind_context: UnwindContext,
    cache: Option<InMemoryCache>,
}

impl<T: Module> UnwindModule<T> {
    pub(crate) fn new(mut module: T, pic_eh_frame: bool, cache: Option<InMemoryCache>) -> Self {
        let unwind_context = UnwindContext::new(&mut module, pic_eh_frame);
        UnwindModule { module, unwind_context, cache }
    }
}

impl UnwindModule<ObjectModule> {
    pub(crate) fn finish(self) -> ObjectProduct {
        let mut product = self.module.finish();
        self.unwind_context.emit(&mut product);
        product
    }
}

#[cfg(feature = "jit")]
impl UnwindModule<cranelift_jit::JITModule> {
    pub(crate) fn finalize_definitions(mut self) -> cranelift_jit::JITModule {
        self.module.finalize_definitions().unwrap();
        unsafe { self.unwind_context.register_jit(&self.module) };
        self.module
    }
}

impl<T: Module> Module for UnwindModule<T> {
    fn isa(&self) -> &dyn TargetIsa {
        self.module.isa()
    }

    fn declarations(&self) -> &ModuleDeclarations {
        self.module.declarations()
    }

    fn get_name(&self, name: &str) -> Option<FuncOrDataId> {
        self.module.get_name(name)
    }

    fn target_config(&self) -> TargetFrontendConfig {
        self.module.target_config()
    }

    fn declare_function(
        &mut self,
        name: &str,
        linkage: Linkage,
        signature: &Signature,
    ) -> ModuleResult<FuncId> {
        self.module.declare_function(name, linkage, signature)
    }

    fn declare_anonymous_function(&mut self, signature: &Signature) -> ModuleResult<FuncId> {
        self.module.declare_anonymous_function(signature)
    }

    fn declare_data(
        &mut self,
        name: &str,
        linkage: Linkage,
        writable: bool,
        tls: bool,
    ) -> ModuleResult<DataId> {
        self.module.declare_data(name, linkage, writable, tls)
    }

    fn declare_anonymous_data(&mut self, writable: bool, tls: bool) -> ModuleResult<DataId> {
        self.module.declare_anonymous_data(writable, tls)
    }

    fn define_function_with_control_plane(
        &mut self,
        func: FuncId,
        ctx: &mut Context,
        ctrl_plane: &mut ControlPlane,
    ) -> ModuleResult<()> {
        let res;
        let res = if let Some(cache) = &mut self.cache {
            if ctx.func.layout.blocks().nth(1).is_none()
                || ctx.func.layout.blocks().nth(2).is_none()
            {
                ctx.compile(self.module.isa(), ctrl_plane)?;
                ctx.compiled_code().unwrap()
            } else {
                res = compile_with_cache(&mut self.module, ctx, ctrl_plane, cache)?;
                &res
            }
        } else {
            ctx.compile(self.module.isa(), ctrl_plane)?;
            ctx.compiled_code().unwrap()
        };

        let alignment = res.buffer.alignment as u64;
        let relocs = res
            .buffer
            .relocs()
            .iter()
            .map(|reloc| ModuleReloc::from_mach_reloc(&reloc, &ctx.func, func))
            .collect::<Vec<_>>();
        self.module.define_function_bytes(func, alignment, res.buffer.data(), &relocs)?;

        self.unwind_context.add_function(&mut self.module, func, res);
        Ok(())
    }

    fn define_function_bytes(
        &mut self,
        _func_id: FuncId,
        _alignment: u64,
        _bytes: &[u8],
        _relocs: &[ModuleReloc],
    ) -> ModuleResult<()> {
        unimplemented!()
    }

    fn define_data(&mut self, data_id: DataId, data: &DataDescription) -> ModuleResult<()> {
        self.module.define_data(data_id, data)
    }
}

#[inline(never)]
fn compile_with_cache(
    module: &mut impl Module,
    ctx: &mut Context,
    ctrl_plane: &mut ControlPlane,
    cache: &mut InMemoryCache,
) -> Result<CompiledCode, cranelift_module::ModuleError> {
    let isa: &dyn TargetIsa = module.isa();
    let cache_key_hash = {
        let _tt = cranelift_codegen::timing::try_incremental_cache();

        let cache_key_hash =
            cranelift_codegen::incremental_cache::compute_cache_key(isa, &ctx.func);

        if let Some(stencil) = cache.get(&cache_key_hash) {
            return Ok(stencil.apply_params(&ctx.func.params));
        }

        cache_key_hash
    };
    let stencil = ctx
        .compile_stencil(isa, ctrl_plane)
        .map_err(|err| cranelift_codegen::CompileError { inner: err, func: &ctx.func })?;
    {
        let _tt = cranelift_codegen::timing::store_incremental_cache();
        cache.insert(cache_key_hash, stencil.clone());
    };
    Ok(stencil.apply_params(&ctx.func.params))
}

#[derive(Clone, Default)]
pub struct InMemoryCache(Arc<RwLock<HashMap<CacheKeyHash, IntoDynSyncSend<CompiledCodeStencil>>>>);

impl InMemoryCache {
    fn get(&self, key: &CacheKeyHash) -> Option<CompiledCodeStencil> {
        self.0.read().get(key).cloned().map(|val| val.0)
    }

    fn insert(&mut self, key: CacheKeyHash, val: CompiledCodeStencil) {
        self.0.write().insert(key, IntoDynSyncSend(val));
    }
}

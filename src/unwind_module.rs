use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::Signature;
use cranelift_codegen::isa::{TargetFrontendConfig, TargetIsa};
use cranelift_module::{
    DataDescription, DataId, FuncId, FuncOrDataId, Linkage, Module, ModuleDeclarations,
    ModuleReloc, ModuleResult,
};
use cranelift_object::{ObjectModule, ObjectProduct};

use crate::UnwindContext;

/// A wrapper around a [Module] which adds any defined function to the [UnwindContext].
pub(crate) struct UnwindModule<T> {
    pub(crate) module: T,
    unwind_context: UnwindContext,
    funcs: Vec<(FuncId, usize)>,
    datas: Vec<DataId>,
}

impl<T: Module> UnwindModule<T> {
    pub(crate) fn new(mut module: T, pic_eh_frame: bool) -> Self {
        let unwind_context = UnwindContext::new(&mut module, pic_eh_frame);
        UnwindModule { module, unwind_context, funcs: vec![], datas: vec![] }
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
    pub(crate) fn finalize_definitions_ref(&mut self, pic_eh_frame: bool) {
        use std::mem;

        self.module.finalize_definitions().unwrap();
        let unwind_context = mem::replace(
            &mut self.unwind_context,
            UnwindContext::new(&mut self.module, pic_eh_frame),
        );
        unsafe { unwind_context.register_jit(&self.module) };

        let symbols = std::iter::chain(
            self.funcs.drain(..).map(|(func_id, size)| {
                let name = self
                    .module
                    .declarations()
                    .get_function_decl(func_id)
                    .name
                    .as_deref()
                    .unwrap_or("???");
                let addr = self.module.get_finalized_function(func_id).expose_provenance() as u64;
                (name, addr, size as u64, object40::elf::STT_FUNC)
            }),
            self.datas.drain(..).map(|data_id| {
                let name = self
                    .module
                    .declarations()
                    .get_data_decl(data_id)
                    .name
                    .as_deref()
                    .unwrap_or("???");
                let (addr, size) = self.module.get_finalized_data(data_id);
                (name, addr.expose_provenance() as u64, size as u64, object40::elf::STT_OBJECT)
            }),
        )
        .collect::<Vec<_>>();
        let obj = objfile_for_sym(&symbols);
        mem::forget(wasmtime_internal_jit_debug::gdb_jit_int::GdbJitImageRegistration::register(
            obj,
        ));
    }

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
        self.module.define_function_with_control_plane(func, ctx, ctrl_plane)?;
        self.unwind_context.add_function(&mut self.module, func, ctx);
        self.funcs.push((func, ctx.compiled_code().unwrap().code_buffer().len()));
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
        self.module.define_data(data_id, data)?;
        self.datas.push(data_id);
        Ok(())
    }
}

fn objfile_for_sym(symbols: &[(&str, u64, u64, object40::elf::SymbolType)]) -> Vec<u8> {
    use object40::elf::{
        self, ELFOSABI_GNU, EM_X86_64, ET_EXEC, FileFlags, SymbolOther, SymbolSection,
    };
    use object40::write::elf::{FileHeader, SectionHeader, SinglePhaseWriter, Sym};

    let mut obj_data = vec![];
    let mut writer =
        SinglePhaseWriter::new_single_phase(object40::Endianness::Little, true, &mut obj_data);
    writer
        .write_file_header(&FileHeader {
            os_abi: ELFOSABI_GNU,
            abi_version: 1,
            e_type: ET_EXEC,
            e_machine: EM_X86_64,
            e_entry: 0,
            e_flags: FileFlags::default(),
        })
        .unwrap();
    let (_phdr_offset, _phdr_size) = writer.write_program_headers_placeholder(0);

    // .text
    let text_section_index = object40::write::elf::SectionIndex(1);

    // .eh_frame

    // .symtab
    let _symtab_offset = writer.write_null_symbol();
    for (i, &(name, addr, size, type_)) in symbols.iter().enumerate() {
        let name_id = writer.add_string(name.as_bytes());
        let section = Some(text_section_index.0 + i as u32);
        writer.write_symbol(&Sym {
            st_name: writer.string_offset(Some(name_id)),
            section,
            st_info: elf::SymbolInfo::new(elf::STB_LOCAL, type_),
            st_other: SymbolOther::default(),
            st_shndx: SymbolSection::default(), // FIXME
            st_value: addr,
            st_size: size,
        });
    }
    let num_local_symtab = 1u32 + symbols.len() as u32;

    // .strtab
    let (_strtab_offset, _strtab_size) = writer.write_strtab().unwrap();

    // .shstrtab
    let text_name_id = symbols
        .iter()
        .map(|(name, _, _, _)| {
            writer.add_section_name(Box::leak(format!(".text.{name}").into_boxed_str()).as_bytes())
        })
        .collect::<Vec<_>>();
    writer.write_shstrtab().unwrap();

    // section header table
    writer.write_null_section_header();
    for (i, &(_name, addr, size, _type)) in symbols.iter().enumerate() {
        let index = writer.write_section_header(&SectionHeader {
            sh_name: writer.section_name_offset(Some(text_name_id[i])),
            sh_type: elf::SHT_NOBITS,
            sh_flags: elf::SHF_ALLOC | elf::SHF_EXECINSTR,
            sh_addr: addr,
            sh_offset: 0,
            sh_size: size,
            sh_link: 0,
            sh_info: 0,
            sh_addralign: 1,
            sh_entsize: 0,
        });
        debug_assert_eq!(index.0, text_section_index.0 + i as u32);
    }
    writer.write_strtab_section_header();
    writer.write_symtab_section_header(num_local_symtab);
    writer.write_shstrtab_section_header();

    let program_headers = [];
    let mut header_buf = Vec::new();
    writer.write_headers_to(&mut header_buf, &program_headers).unwrap();
    obj_data[..header_buf.len()].copy_from_slice(&header_buf);

    obj_data
}

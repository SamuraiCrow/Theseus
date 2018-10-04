//! Routines for parsing the `nano_core`, the fully-linked, already loaded code that is currently executing.
//! As such, it performs no loading, but rather just creates metadata that represents
//! the existing kernel code that was loaded by the bootloader, and adds those functions to the system map.

use core::ops::DerefMut;
use alloc::{BTreeMap, String};
use alloc::arc::Arc;
use alloc::string::ToString;
use spin::Mutex;
use cow_arc::CowArc;

use xmas_elf;
use xmas_elf::ElfFile;
use xmas_elf::sections::ShType;
use xmas_elf::sections::{SHF_WRITE, SHF_ALLOC, SHF_EXECINSTR};

use memory::{FRAME_ALLOCATOR, ModuleArea, get_module, MemoryManagementInfo, Frame, PageTable, VirtualAddress, MappedPages, EntryFlags, allocate_pages_by_bytes};
use metadata::{CrateType, LoadedCrate, StrongCrateRef, LoadedSection, StrongSectionRef, SectionType};


/// Decides which parsing technique to use, either the symbol file or the actual binary file.
/// `parse_nano_core_binary()` is VERY SLOW in debug mode for large binaries, so we use the more efficient symbol file parser instead.
/// Note that this must match the setup of the kernel/Makefile as well as the cfg/grub.cfg entry
const PARSE_NANO_CORE_SYMBOL_FILE: bool = true;



/// Just like Rust's `try!()` macro, but packages up the given error message in a tuple
/// with the array of 3 MappedPages that must also be returned. 
macro_rules! try_mp {
    ($expr:expr, $tp:expr, $rp:expr, $dp:expr) => (match $expr {
        Ok(val) => val,
        Err(err_msg) => return Err((err_msg, [$tp, $rp, $dp])),
    });
}


/// Just like Rust's `try!()` macro, but sets the given error message
/// and then breaks out of the loop;
/// with the array of 3 MappedPages that must also be returned. 
macro_rules! try_break {
    ($expr:expr, $result:ident) => (match $expr {
        Ok(val) => val,
        Err(err) => {
            $result = Err(err);
            break;
        }
    });
}


/// Parses the nano_core module that represents the already loaded (and currently running) nano_core code.
/// Basically, just searches for global (public) symbols, which are added to the system map and the crate metadata.
/// 
/// If an error occurs, the returned `Result::Err` contains the passed-in `text_pages`, `rodata_pages`, and `data_pages`
/// because those cannot be dropped, as they hold the currently-running code, and dropping them would cause endless exceptions.
pub fn parse_nano_core(
    kernel_mmi:   &mut MemoryManagementInfo, 
    text_pages:   MappedPages, 
    rodata_pages: MappedPages, 
    data_pages:   MappedPages, 
    verbose_log: bool
) -> Result<usize, (&'static str, [Arc<Mutex<MappedPages>>; 3])> {

    let text_pages   = Arc::new(Mutex::new(text_pages));
    let rodata_pages = Arc::new(Mutex::new(rodata_pages));
    let data_pages   = Arc::new(Mutex::new(data_pages));

    let crate_name = String::from("nano_core");
    debug!("parse_nano_core: trying to load and parse the nano_core file");
    let module = try_mp!(
        get_module(&format!("{}nano_core", CrateType::Kernel.prefix())).ok_or("Couldn't find nano_core module"), 
        text_pages, rodata_pages, data_pages
    );
    use kernel_config::memory::address_is_page_aligned;
    if !address_is_page_aligned(module.start_address()) {
        error!("module {} is not page aligned!", module.name());
        return Err(("nano_core module was not page aligned", [text_pages, rodata_pages, data_pages]));
    } 

    // below, we map the nano_core module file just so we can parse it. We don't need to actually load it since we're already running it.
    if let PageTable::Active(ref mut active_table) = kernel_mmi.page_table {
        let (size, flags) = if PARSE_NANO_CORE_SYMBOL_FILE {
            (
                // + 1 to add space for appending a null character to the end of the symbol file string
                module.size() + 1, 
                // WRITABLE because we need to write that null character
                EntryFlags::PRESENT | EntryFlags::WRITABLE
            )
        }
        else {
            (module.size(), EntryFlags::PRESENT)
        };

        let temp_module_mapping = {
            let new_pages = try_mp!(allocate_pages_by_bytes(size).ok_or("couldn't allocate pages for nano_core module"), text_pages, rodata_pages, data_pages);
            let mut frame_allocator = try_mp!(FRAME_ALLOCATOR.try().ok_or("couldn't get FRAME_ALLOCATOR"), text_pages, rodata_pages, data_pages).lock();
            try_mp!(active_table.map_allocated_pages_to(
                new_pages, Frame::range_inclusive_addr(module.start_address(), size), 
                flags, frame_allocator.deref_mut()
            ), text_pages, rodata_pages, data_pages)
        };

        let new_crate_ref = if PARSE_NANO_CORE_SYMBOL_FILE {
            parse_nano_core_symbol_file(temp_module_mapping, module, text_pages, rodata_pages, data_pages, size)?
        } else {
            parse_nano_core_binary(temp_module_mapping, module, text_pages, rodata_pages, data_pages, size)?
        };

        let default_namespace = super::get_default_namespace();
        trace!("parse_nano_core(): adding symbols to namespace...");
        let new_syms = default_namespace.add_symbols(new_crate_ref.lock_as_ref().sections.values(), verbose_log);
        trace!("parse_nano_core(): finished adding symbols.");
        default_namespace.crate_tree.lock().insert(crate_name, new_crate_ref);
        info!("parsed nano_core crate, {} new symbols.", new_syms);
        Ok(new_syms)

        // temp_module_mapping is automatically unmapped when it falls out of scope here (frame allocator must not be locked)
    }
    else {
        error!("parse_nano_core(): error getting kernel's active page table to map module.");
        Err(("couldn't get kernel's active page table", [text_pages, rodata_pages, data_pages]))
    }
}



/// Parses the nano_core symbol file that represents the already loaded (and currently running) nano_core code.
/// Basically, just searches for global (public) symbols, which are added to the system map and the crate metadata.
/// 
/// Drops the given `mapped_pages` that hold the nano_core module file itself.
fn parse_nano_core_symbol_file(
    mut mapped_pages: MappedPages,
    nano_core_object_file: &'static ModuleArea,
    text_pages:   Arc<Mutex<MappedPages>>,
    rodata_pages: Arc<Mutex<MappedPages>>,
    data_pages:   Arc<Mutex<MappedPages>>,
    size: usize
) -> Result<StrongCrateRef, (&'static str, [Arc<Mutex<MappedPages>>; 3])> {
    let crate_name = String::from("nano_core");
    debug!("Parsing nano_core symbols: size {:#x}({}), mapped_pages: {:?}, text_pages: {:?}, rodata_pages: {:?}, data_pages: {:?}", 
        size, size, mapped_pages, text_pages, rodata_pages, data_pages);

    // because the nano_core doesn't have one section per function/data/rodata, we fake it here with an arbitrary section counter
    let mut section_counter = 0;
    let mut sections: BTreeMap<usize, StrongSectionRef> = BTreeMap::new();

    // ensure that there's a null byte at the end
    {
        let null_byte: &mut u8 = try_mp!(mapped_pages.as_type_mut(size - 1), text_pages, rodata_pages, data_pages);
        *null_byte = 0u8;
    }

    let new_crate = CowArc::new(LoadedCrate {
        crate_name:   crate_name, 
        object_file:  nano_core_object_file,
        sections:     BTreeMap::new(),
        text_pages:   Some(text_pages.clone()),
        rodata_pages: Some(rodata_pages.clone()),
        data_pages:   Some(data_pages.clone()),
    });
    let new_crate_weak_ref = CowArc::downgrade(&new_crate);

    // scoped to drop the borrow on mapped_pages through `bytes`
    {
        use util::c_str::CStr;
        let bytes = try_mp!(mapped_pages.as_slice_mut(0, size), text_pages, rodata_pages, data_pages);
        let symbol_cstr = try_mp!(CStr::from_bytes_with_nul(bytes).map_err(|e| {
            error!("parse_nano_core_symbol_file(): error casting memory to CStr: {:?}", e);
            "FromBytesWithNulError occurred when casting nano_core symbol memory to CStr"
        }), text_pages, rodata_pages, data_pages);
        let symbol_str = try_mp!(symbol_cstr.to_str().map_err(|e| {
            error!("parse_nano_core_symbol_file(): error with CStr::to_str(): {:?}", e);
            "Utf8Error occurred when parsing nano_core symbols CStr"
        }), text_pages, rodata_pages, data_pages);

        // debug!("========================= NANO_CORE SYMBOL STRING ========================\n{}", symbol_str);


        let mut text_shndx:   Option<usize> = None;
        let mut data_shndx:   Option<usize> = None;
        let mut rodata_shndx: Option<usize> = None;
        let mut bss_shndx:    Option<usize> = None;

        
        // a closure that parses a section index out of a string like "[7]"
        let parse_section_ndx = |str_ref: &str| {
            let open  = str_ref.find("[");
            let close = str_ref.find("]");
            open.and_then(|start| close.and_then(|end| str_ref.get((start + 1) .. end)))
                .and_then(|t| t.trim().parse::<usize>().ok())
        };

        // first, find the section indices that we care about: .text, .data, .rodata, and .bss
        let file_iterator = symbol_str.lines().enumerate();
        for (_line_num, line) in file_iterator.clone() {

            // skip empty lines
            let line = line.trim();
            if line.is_empty() { continue; }

            // debug!("Looking at line: {:?}", line);

            if line.contains(".text") && line.contains("PROGBITS") {
                text_shndx = parse_section_ndx(line);
            }
            else if line.contains(".data") && line.contains("PROGBITS") {
                data_shndx = parse_section_ndx(line);
            }
            else if line.contains(".rodata") && line.contains("PROGBITS") {
                rodata_shndx = parse_section_ndx(line);
            }
            else if line.contains(".bss") && line.contains("NOBITS") {
                bss_shndx = parse_section_ndx(line);
            }

            // once we've found the 4 sections we care about, we're done
            if text_shndx.is_some() && rodata_shndx.is_some() && data_shndx.is_some() && bss_shndx.is_some() {
                break;
            }
        }

        let text_shndx   = try_mp!(text_shndx  .ok_or("parse_nano_core_symbol_file(): couldn't find .text section index"),   text_pages, rodata_pages, data_pages);
        let rodata_shndx = try_mp!(rodata_shndx.ok_or("parse_nano_core_symbol_file(): couldn't find .rodata section index"), text_pages, rodata_pages, data_pages);
        let data_shndx   = try_mp!(data_shndx  .ok_or("parse_nano_core_symbol_file(): couldn't find .data section index"),   text_pages, rodata_pages, data_pages);
        let bss_shndx    = try_mp!(bss_shndx   .ok_or("parse_nano_core_symbol_file(): couldn't find .bss section index"),    text_pages, rodata_pages, data_pages);


        // second, skip ahead to the start of the symbol table 
        let mut file_iterator = file_iterator.skip_while( | (_line_num, line) |  {
            !line.starts_with("Symbol table")
        });
        // skip the symbol table start line, e.g., "Symbol table '.symtab' contains N entries:"
        if let Some((_num, _line)) = file_iterator.next() {
            // trace!("SKIPPING LINE {}: {}", _num + 1, _line);
        }
        // skip one more line, the line with the column headers, e.g., "Num:     Value     Size Type   Bind   Vis ..."
        if let Some((_num, _line)) = file_iterator.next() {
            // trace!("SKIPPING LINE {}: {}", _num + 1, _line);
        }

        // a placeholder for any error that might occur during the loop below
        let mut loop_result: Result<(), &'static str> = Ok(());

        {
            let text_pages_locked = text_pages.lock();
            let rodata_pages_locked = rodata_pages.lock();
            let data_pages_locked = data_pages.lock();

            // third, parse each symbol table entry, which should all have "GLOBAL" bindings
            for (_line_num, line) in file_iterator {
                if line.is_empty() { continue; }
                
                // we need the following items from a symbol table entry:
                // * Value (address),      column 1
                // * Size,                 column 2
                // * Ndx,                  column 6
                // * DemangledName#hash    column 7 to end

                // Can't use split_whitespace() here, because we need to splitn and then get the remainder of the line
                // after we've split the first 7 columns by whitespace. So we write a custom closure to group multiple whitespaces together.\
                // We use "splitn(8, ..)" because it stops at the 8th column (column index 7) and gets the rest of the line in a single iteration.
                let mut prev_whitespace = true; // by default, we start assuming that the previous element was whitespace.
                let mut parts = line.splitn(8, |c: char| {
                    if c.is_whitespace() {
                        if prev_whitespace {
                            false
                        } else {
                            prev_whitespace = true;
                            true
                        }
                    } else {
                        prev_whitespace = false;
                        false
                    }
                }).map(str::trim);

                let _num      = try_break!(parts.next().ok_or("parse_nano_core_symbol_file(): couldn't get column 0 'Num'"),   loop_result);
                let sec_vaddr = try_break!(parts.next().ok_or("parse_nano_core_symbol_file(): couldn't get column 1 'Value'"), loop_result);
                let sec_size  = try_break!(parts.next().ok_or("parse_nano_core_symbol_file(): couldn't get column 2 'Size'"),  loop_result);
                let _typ      = try_break!(parts.next().ok_or("parse_nano_core_symbol_file(): couldn't get column 3 'Type'"),  loop_result);
                let _bind     = try_break!(parts.next().ok_or("parse_nano_core_symbol_file(): couldn't get column 4 'Bind'"),  loop_result);
                let _vis      = try_break!(parts.next().ok_or("parse_nano_core_symbol_file(): couldn't get column 5 'Vis'"),   loop_result);
                let sec_ndx   = try_break!(parts.next().ok_or("parse_nano_core_symbol_file(): couldn't get column 6 'Ndx'"),   loop_result);
                let name_hash = try_break!(parts.next().ok_or("parse_nano_core_symbol_file(): couldn't get column 7 'Name'"),  loop_result);

                // According to the operation of the tool "demangle_readelf_file", the last 'Name' column
                // consists of the already demangled name (which may have spaces) and then an optional hash,
                // which looks like the following:  NAME#HASH.
                // If there is no hash, then it will just be:   NAME
                // Thus, we need to split "name_hash"  at the '#', if it exists
                let (no_hash, hash) = {
                    let mut tokens = name_hash.split("#");
                    let no_hash = try_break!(tokens.next().ok_or("parse_nano_core_symbol_file(): 'Name' column had extraneous '#' characters."), loop_result);
                    let hash = tokens.next();
                    if tokens.next().is_some() {
                        error!("parse_nano_core_symbol_file(): 'Name' column \"{}\" had multiple '#' characters, expected only one as the hash separator!", name_hash);
                        try_break!(Err("parse_nano_core_symbol_file(): 'Name' column had multiple '#' characters, expected only one '#' as the hash separator!"), loop_result);
                    }
                    (no_hash.to_string(), hash.map(str::to_string))
                };
                
                let sec_vaddr = try_break!(usize::from_str_radix(sec_vaddr, 16)
                    .map_err(|e| {
                        error!("parse_nano_core_symbol_file(): error parsing virtual address Value at line {}: {:?}\n    line: {}", _line_num + 1, e, line);
                        "parse_nano_core_symbol_file(): couldn't parse virtual address (value column)"
                    }),
                    loop_result
                );
                let sec_size = try_break!(usize::from_str_radix(sec_size, 10)
                    .or_else(|e| {
                        sec_size.get(2 ..).ok_or(e).and_then(|sec_size_hex| 
                            usize::from_str_radix(sec_size_hex, 16)
                        )
                    })
                    .map_err(|e| {
                        error!("parse_nano_core_symbol_file(): error parsing size at line {}: {:?}\n    line: {}", _line_num + 1, e, line);
                        "parse_nano_core_symbol_file(): couldn't parse size column"
                    }),
                    loop_result
                ); 

                // while vaddr and size are required, ndx could be valid or not. 
                let sec_ndx = match usize::from_str_radix(sec_ndx, 10) {
                    // If ndx is a valid number, proceed on. 
                    Ok(ndx) => ndx,
                    // Otherwise, if ndx is not a number (e.g., "ABS"), then we just skip that entry (go onto the next line). 
                    _ => {
                        trace!("parse_nano_core_symbol_file(): skipping line {}: {}", _line_num + 1, line);
                        continue;
                    }
                };

                // debug!("parse_nano_core_symbol_file(): name: {}, hash: {:?}, vaddr: {:#X}, size: {:#X}, sec_ndx {}", no_hash, hash, sec_vaddr, sec_size, sec_ndx);

                if sec_ndx == text_shndx {
                    sections.insert(
                        section_counter,
                        Arc::new(Mutex::new(LoadedSection::new(
                            SectionType::Text,
                            no_hash,
                            hash,
                            Arc::clone(&text_pages),
                            try_break!(text_pages_locked.offset_of_address(sec_vaddr).ok_or("nano_core text section wasn't covered by its mapped pages!"), loop_result), 
                            sec_vaddr,
                            sec_size,
                            true,
                            new_crate_weak_ref.clone(), 
                        )))
                    );
                }
                else if sec_ndx == rodata_shndx {
                    sections.insert(
                        section_counter,
                        Arc::new(Mutex::new(LoadedSection::new(
                            SectionType::Rodata,
                            no_hash,
                            hash,
                            Arc::clone(&rodata_pages),
                            try_break!(rodata_pages_locked.offset_of_address(sec_vaddr).ok_or("nano_core rodata section wasn't covered by its mapped pages!"), loop_result),
                            sec_vaddr,
                            sec_size,
                            true,
                            new_crate_weak_ref.clone(),
                        )))
                    );
                }
                else if sec_ndx == data_shndx {
                    sections.insert(
                        section_counter,
                        Arc::new(Mutex::new(LoadedSection::new(
                            SectionType::Data,
                            no_hash,
                            hash,
                            Arc::clone(&data_pages),
                            try_break!(data_pages_locked.offset_of_address(sec_vaddr).ok_or("nano_core data section wasn't covered by its mapped pages!"), loop_result),
                            sec_vaddr,
                            sec_size,
                            true,
                            new_crate_weak_ref.clone(),
                        )))
                    );
                }
                else if sec_ndx == bss_shndx {
                    sections.insert(
                        section_counter,
                        Arc::new(Mutex::new(LoadedSection::new(
                            SectionType::Bss,
                            no_hash,
                            hash,
                            Arc::clone(&data_pages),
                            try_break!(data_pages_locked.offset_of_address(sec_vaddr).ok_or("nano_core bss section wasn't covered by its mapped pages!"), loop_result),
                            sec_vaddr,
                            sec_size,
                            true,
                            new_crate_weak_ref.clone(),
                        )))
                    );
                }
                else {
                    trace!("parse_nano_core_symbol_file(): skipping sec[{}] (probably in .init): name: {}, vaddr: {:#X}, size: {:#X}", sec_ndx, no_hash, sec_vaddr, sec_size);
                }

                section_counter += 1;

            } // end of loop over all lines
        }

        // check to see if we had an error in the above loop
        try_mp!(loop_result, text_pages, rodata_pages, data_pages);
        trace!("parse_nano_core_symbol_file(): finished looping over symtab.");

    } // drops the borrow of `bytes` (and mapped_pages)
    
    // set the new_crate's sections list, since we didn't do it earlier
    {
        let mut new_crate_mut = try_mp!(
            new_crate.lock_as_mut()
                .ok_or_else(|| "BUG: parse_nano_core_symbol_file(): couldn't get exclusive mutable access to new_crate"),
            text_pages, rodata_pages, data_pages
        );
        new_crate_mut.sections = sections;
    }
    
    Ok(new_crate)
}




/// Parses the nano_core ELF binary file, which is already loaded and running.  
/// Thus, we simply search for its global symbols, and add them to the system map and the crate metadata.
/// 
/// Drops the given `mapped_pages` that hold the nano_core binary file itself.
fn parse_nano_core_binary(
    mapped_pages: MappedPages, 
    nano_core_object_file: &'static ModuleArea,
    text_pages:   Arc<Mutex<MappedPages>>, 
    rodata_pages: Arc<Mutex<MappedPages>>, 
    data_pages:   Arc<Mutex<MappedPages>>, 
    size_in_bytes: usize
) -> Result<StrongCrateRef, (&'static str, [Arc<Mutex<MappedPages>>; 3])> {
    let crate_name = String::from("nano_core");
    debug!("Parsing {} binary: size {:#x}({}), MappedPages: {:?}, text_pages: {:?}, rodata_pages: {:?}, data_pages: {:?}", 
            crate_name, size_in_bytes, size_in_bytes, mapped_pages, text_pages, rodata_pages, data_pages);

    let byte_slice: &[u8] = try_mp!(mapped_pages.as_slice(0, size_in_bytes), text_pages, rodata_pages, data_pages);
    let elf_file = try_mp!(ElfFile::new(byte_slice), text_pages, rodata_pages, data_pages); // returns Err(&str) if ELF parse fails

    // For us to properly load the ELF file, it must NOT have been stripped,
    // meaning that it must still have its symbol table section. Otherwise, relocations will not work.
    use xmas_elf::sections::SectionData::SymbolTable64;
    let sssec = elf_file.section_iter().filter(|sec| sec.get_type() == Ok(ShType::SymTab)).next();
    let symtab_data = match sssec.ok_or("no symtab section").and_then(|s| s.get_data(&elf_file)) {
        Ok(SymbolTable64(symtab)) => Ok(symtab),
        _ => {
            error!("parse_nano_core_binary(): can't load file: no symbol table found. Was file stripped?");
            Err("cannot load nano_core: no symbol table found. Was file stripped?")
        }
    };
    let symtab = try_mp!(symtab_data, text_pages, rodata_pages, data_pages);
    // debug!("symtab: {:?}", symtab);
    
    // find the .text, .data, and .rodata sections
    let mut text_shndx:   Option<usize> = None;
    let mut rodata_shndx: Option<usize> = None;
    let mut data_shndx:   Option<usize> = None;
    let mut bss_shndx:    Option<usize> = None;

    let mut loop_result: Result<(), &'static str> = Ok(());
    
    for (shndx, sec) in elf_file.section_iter().enumerate() {
        // trace!("parse_nano_core_binary(): looking at sec[{}]: {:?}", shndx, sec);
        // the PROGBITS sections are the bulk of what we care about, i.e., .text & data sections
        if let Ok(ShType::ProgBits) = sec.get_type() {
            // skip null section and any empty sections
            let sec_size = sec.size() as usize;
            if sec_size == 0 { continue; }

            if let Ok(name) = sec.get_name(&elf_file) {
                match name {
                    ".text" => {
                        if !(sec.flags() & (SHF_ALLOC | SHF_WRITE | SHF_EXECINSTR) == (SHF_ALLOC | SHF_EXECINSTR)) {
                            try_break!(Err(".text section had wrong flags!"), loop_result);
                        }
                        text_shndx = Some(shndx);
                    }
                    ".data" => {
                        if !(sec.flags() & (SHF_ALLOC | SHF_WRITE | SHF_EXECINSTR) == (SHF_ALLOC | SHF_WRITE)) {
                            try_break!(Err(".data section had wrong flags!"), loop_result);
                        }
                        data_shndx = Some(shndx);
                    }
                    ".rodata" => {
                        if !(sec.flags() & (SHF_ALLOC | SHF_WRITE | SHF_EXECINSTR) == (SHF_ALLOC)) {
                            try_break!(Err(".rodata section had wrong flags!"), loop_result);
                        }
                        rodata_shndx = Some(shndx);
                    }
                    _ => {
                        continue;
                    }
                };
            }
        }
        // look for .bss section
        else if let Ok(ShType::NoBits) = sec.get_type() {
            // skip null section and any empty sections
            let sec_size = sec.size() as usize;
            if sec_size == 0 { continue; }

            if let Ok(name) = sec.get_name(&elf_file) {
                if name == ".bss" {
                    if !(sec.flags() & (SHF_ALLOC | SHF_WRITE | SHF_EXECINSTR) == (SHF_ALLOC | SHF_WRITE)) {
                        try_break!(Err(".bss section had wrong flags!"), loop_result);
                    }
                    bss_shndx = Some(shndx);
                }
            }
        }

        // // once we've found the 4 sections we care about, skip the rest.
        // if text_shndx.is_some() && rodata_shndx.is_some() && data_shndx.is_some() && bss_shndx.is_some() {
        //     break;
        // }
    }

    try_mp!(loop_result, text_pages, rodata_pages, data_pages);

    let text_shndx   = try_mp!(text_shndx.ok_or("couldn't find .text section in nano_core ELF"), text_pages, rodata_pages, data_pages);
    let rodata_shndx = try_mp!(rodata_shndx.ok_or("couldn't find .rodata section in nano_core ELF"), text_pages, rodata_pages, data_pages);
    let data_shndx   = try_mp!(data_shndx.ok_or("couldn't find .data section in nano_core ELF"), text_pages, rodata_pages, data_pages);
    let bss_shndx    = try_mp!(bss_shndx.ok_or("couldn't find .bss section in nano_core ELF"), text_pages, rodata_pages, data_pages);

    let new_crate = CowArc::new(LoadedCrate {
        crate_name:   crate_name, 
        object_file:  nano_core_object_file,
        sections:     BTreeMap::new(),
        text_pages:   Some(text_pages.clone()),
        rodata_pages: Some(rodata_pages.clone()),
        data_pages:   Some(data_pages.clone()),
    });
    let new_crate_weak_ref = CowArc::downgrade(&new_crate);

    let mut loop_result: Result<(), &'static str> = Ok(());
    
    let sections = {
        let text_pages_locked = text_pages.lock();
        let rodata_pages_locked = rodata_pages.lock();
        let data_pages_locked = data_pages.lock();

        // Iterate through the symbol table so we can find which sections are global (publicly visible).
        // As the nano_core doesn't have one section per function/data/rodata, we fake it here with an arbitrary section counter.
        let mut section_counter = 0;
        let mut sections: BTreeMap<usize, StrongSectionRef> = BTreeMap::new();

        use xmas_elf::symbol_table::Entry;
        for entry in symtab.iter() {
            // public symbols can have any visibility setting, but it's the binding that matters (must be GLOBAL)

            // use xmas_elf::symbol_table::Visibility;
            // match entry.get_other() {
            //     Visibility::Default | Visibility::Hidden => {
            //         // do nothing, fall through to proceed
            //     }
            //     _ => {
            //         continue; // skip this
            //     }
            // };
            
            if let Ok(bind) = entry.get_binding() {
                if bind == xmas_elf::symbol_table::Binding::Global {
                    if let Ok(typ) = entry.get_type() {
                        if typ == xmas_elf::symbol_table::Type::Func || typ == xmas_elf::symbol_table::Type::Object {
                            let sec_vaddr = entry.value() as VirtualAddress;
                            let sec_size = entry.size() as usize;
                            let name = try_break!(entry.get_name(&elf_file), loop_result);

                            let demangled = super::demangle_symbol(name);
                            // debug!("parse_nano_core_binary(): name: {}, demangled: {}, vaddr: {:#X}, size: {:#X}", name, demangled.no_hash, sec_vaddr, sec_size);

                            let new_section = {
                                if entry.shndx() as usize == text_shndx {
                                    Some(LoadedSection::new(
                                        SectionType::Text,
                                        demangled.no_hash,
                                        demangled.hash,
                                        Arc::clone(&text_pages),
                                        try_break!(text_pages_locked.offset_of_address(sec_vaddr).ok_or("nano_core text section wasn't covered by its mapped pages!"), loop_result),
                                        sec_vaddr,
                                        sec_size,
                                        true,
                                        new_crate_weak_ref.clone(),
                                    ))
                                }
                                else if entry.shndx() as usize == rodata_shndx {
                                    Some(LoadedSection::new(
                                        SectionType::Rodata,
                                        demangled.no_hash,
                                        demangled.hash,
                                        Arc::clone(&rodata_pages),
                                        try_break!(rodata_pages_locked.offset_of_address(sec_vaddr).ok_or("nano_core rodata section wasn't covered by its mapped pages!"), loop_result),
                                        sec_vaddr,
                                        sec_size,
                                        true,
                                        new_crate_weak_ref.clone(),
                                    ))
                                }
                                else if entry.shndx() as usize == data_shndx {
                                    Some(LoadedSection::new(
                                        SectionType::Data,
                                        demangled.no_hash,
                                        demangled.hash,
                                        Arc::clone(&data_pages),
                                        try_break!(data_pages_locked.offset_of_address(sec_vaddr).ok_or("nano_core data section wasn't covered by its mapped pages!"), loop_result),
                                        sec_vaddr,
                                        sec_size,
                                        true,
                                        new_crate_weak_ref.clone(),
                                    ))
                                }
                                else if entry.shndx() as usize == bss_shndx {
                                    Some(LoadedSection::new(
                                        SectionType::Bss,
                                        demangled.no_hash,
                                        demangled.hash,
                                        Arc::clone(&data_pages),
                                        try_break!(data_pages_locked.offset_of_address(sec_vaddr).ok_or("nano_core bss section wasn't covered by its mapped pages!"), loop_result),
                                        sec_vaddr,
                                        sec_size,
                                        true,
                                        new_crate_weak_ref.clone(),
                                    ))
                                }
                                else {
                                    error!("Unexpected entry.shndx(): {}", entry.shndx());
                                    None
                                }
                            };

                            if let Some(sec) = new_section {
                                // debug!("parse_nano_core: new section: {:?}", sec);
                                sections.insert(section_counter, Arc::new(Mutex::new(sec)));
                                section_counter += 1;
                            }
                        }
                    }
                }
            }
        }
        sections 
    };

    // check if there was an error in the loop above
    try_mp!(loop_result, text_pages, rodata_pages, data_pages);

    // set the new_crate's sections list, since we didn't do it earlier
    {
        let mut new_crate_mut = try_mp!(
            new_crate.lock_as_mut()
                .ok_or_else(|| "BUG: parse_nano_core_binary(): couldn't get exclusive mutable access to new_crate"),
            text_pages, rodata_pages, data_pages
        );
        new_crate_mut.sections = sections;
    }

    Ok(new_crate)
}
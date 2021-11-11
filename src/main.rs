use std::path::Path;
use std::cell::RefCell;
use std::rc::Rc;

// yaxpeax-arch defines `Arch`, a trait collecting relevant types to reason about a specific
// architecture: the type of instructions, addresses, operands, and decoders for the architecture's
// instruction set.
use yaxpeax_arch::Arch;
// because an `Arch` may have a nontrivial address (for example, segmented memory spaces),
// `AddressDisplay` impls are required to display an `Arch::Address` generically.
use yaxpeax_arch::AddressDisplay;

// here, we're going to look at amd64/x86_64 code specifically. `yaxpeax_x86::long_mode::Arch`
// implements `yaxpeax_arch::Arch`, so reimport it by a shorter name for reuse later.
use yaxpeax_x86::long_mode::{Arch as AMD64};

// yaxpeax_core is loosely designed around analyses yielding series of updates, which are then
// merged into a wider understanding of a program. `BaseUpdate` describes some kinds of information
// that can be applied in an architecture-independent manner: adding a comment for code or data,
// defining a symbol or function at an address, or some architecture-specialized update that a
// particular architecture's analyses understands.
use yaxpeax_core::arch::BaseUpdate;
// yaxpeax_core has a baked-in notion of functions and symbols. a `Symbol` is a name for some data,
// somewhere - it's a pair of a Library specifying where it can be located, and a name in that
// library. independent of the `Symbol` itself is the address it can be found at, in this or some
// other library. this is specifically to allow `Symbol` to describe dynamically linked items.
//
// yaxpeax_core's `Function` is abstract from the actual layout and ABI details - it's intended to
// describe, for example, `ssize_t write(int fd, const void* buf, size_t count)`, rather than the
// _implementation_ of that function as exists in libc or the linux kernel. for implementation
// details, seek a `FunctionImpl` which wraps a `Function` and associates a `FunctionLayout` with
// respect to a specific architecture's description of locations values can be present (registers,
// stack, etc). `FunctionLayout` is not used in this example, but would be where, for example, ABI
// defaults would be relied upon.
use yaxpeax_core::arch::{Function, Library, Symbol};

// yaxpeax_core abstracts over some kinds of ways programs can be described - here, we only care
// about FileRepr::Executable (ELF, PE, MachO - binary containers). an `Executable` has a
// form of `process::ModuleData`, which describes segmented memory spaces as typically seen in
// mapped processes. this is also a useful description of programs in executable binary containers.
use yaxpeax_core::memory::repr::FileRepr;
use yaxpeax_core::memory::repr::process::ModuleData;
// `ModuleInfo` provides metadata around a particular container: this is where you'll find the
// `goblin` header, and some light processing of the data contained in it. sections, imports,
// exports, symbols, and relocations are parceled out (duplicated from the `goblin` header) here. i
// loosely suspect this may be insufficient for dynamic linkage in ELF, but have yet to follow up
// upstream. there is also not (yet) support for binary containers other than ELF or PE. MachO
// should be straightforward. other containers, such as .a/.lib archives, or .class/.jar/.dex
// containers, are a more open question.
use yaxpeax_core::memory::repr::process::ModuleInfo;
// `MemoryRepr` abstracts over the kind of layouts for various files and tries to provide an
// interface that can be read generically. this is primarily interesting in that it's the same
// interface implemented by f.ex `RemoteMemoryRepr`, which is how yaxpeax caches memory when
// connecting to a gdb server for remote debugging/analysis.
use yaxpeax_core::memory::MemoryRepr;

// `InstructionSpan` provides an iterator over the instructions between two addresses.
use yaxpeax_core::arch::InstructionSpan;

// `ContextWrite` is the trait by which `yaxpeax_core::arch::BaseUpdate` or their specialized forms
// can be applied. typically one struct in `arch::<isa>` will implement `ContextWrite`, and is
// where data from analyses is accumulated.
use yaxpeax_core::ContextWrite;

// for x86, that struct is `x86_64Data`.
use yaxpeax_core::arch::x86_64::x86_64Data;

// `InstructionModifiers` allows ad-hoc "i know better than the tool" style annotation for
// specific instruction details. as one example, `InstructionModifiers` is how bounds inferred from
// conditional branches are written - if an instruction is reached from a conditional we may know
// details about a register due to a condition that must have been true. if that same instruction
// was reached by another path, we may know something else, or nothing at all. these bounds are not
// part of a specific instruction, and are encoded as before-instruction constraints that are
// understood in later dfg construction.
use yaxpeax_core::data::modifier::InstructionModifiers;

// `static_single_assignment` really ought to be renamed `data_flow` - SSA is handy but is just one
// approach to construct a dfg for a function. legacy naming i've yet to address.
use yaxpeax_core::analyses::{control_flow, static_single_assignment};
// `yaxpeax_core` can give a good college try at const evaluation, but it's not ideal. i've tried
// to write it in a way that's correct, and sides towards being lossy rather than incorrect, but
// this is not well tested. generally it seems okay but keep a critical eye on constant
// propagation.
use yaxpeax_core::analyses::evaluators::const_evaluator::ConstEvaluator;
// `yaxpeax_core` can also try to infer bounds from condtional branches - `cmp eax, 5; jbe foo`
// implies that at foo, via this branch, `eax <= 5`. this works well for inferring jump table
// bounds and static array limits, but those findings are not threaded back into control flow
// analysis yet.
use yaxpeax_core::analyses::value_range::ConditionalBoundInference;

// a control flow graph. not opinionated about being whole-program or single-function. the only
// distinction between the two are "is control flow to the start of a function an edge to keep?"
// in, for example, calls or tail calls? these have an associated entrypoint and subsequent graph
// of control flow from that address.
use yaxpeax_core::analyses::control_flow::ControlFlowGraph;
// a static single assignment numbering for a program by a given control flow graph. i'd like to
// move towards generically constructing a DFG, with SSA numbering being one approach to building a
// DFG (in contrast to ad-hoc constructions or VSDG or other ideas), but i've not had the time to
// parcel those ideas apart too well.
use yaxpeax_core::analyses::static_single_assignment::SSA;

// for breadth-first search of control flow graphs, useful later.
use petgraph::visit::Bfs;

// `yaxpeax_core`'s general approach to analysis is that analyses under various execution domains
// are implemented, and elsehwere in an architecture-agnostic way, these analyses can be executed.
// consider `two_iteration_function_analysis` below and imagine `AMD64` were a type parameter bound
// by `yaxpeax_arch::Arch` and the appropriate additional bounds. this works and exists elsewhere,
// but i've pared down to x86 for reasons of demonstration.
use yaxpeax_core::arch::x86_64::analyses::evaluators::const_evaluator::ConcreteDomain;
use yaxpeax_core::arch::x86_64::analyses::evaluators::symbolizer::SymbolicDomain;
use yaxpeax_core::arch::x86_64::analyses::evaluators::value_set::ValueSetDomain;
use yaxpeax_core::arch::x86_64::analyses::value_range::conditional_inference::ConditionalInference;

// and a few helpers to display a function when we've done some analyses.
use yaxpeax_arch::ColorSettings;
use yaxpeax_core::arch::display::function::FunctionDisplay;

fn main() {
    // load "/bin/bash"
    let program = yaxpeax_core::memory::reader::load_from_path(Path::new("/bin/bash")).unwrap();
    let program = if let FileRepr::Executable(program) = program {
        program
    } else {
        panic!("/bin/bash was not an executable?");
    };

    // grab some details from the binary and panic if it's not what we expected
    let (isa, entrypoint, imports, exports, symbols) = match (&program as &dyn MemoryRepr<AMD64>).module_info() {
        Some(ModuleInfo::ELF(isa, _, _, _sections, entry, _, imports, exports, symbols)) => {
            (isa, entry, imports, exports, symbols)
        }
        Some(other) => {
            panic!("/bin/bash isn't an elf, but is a {:?}?", other);
        }
        None => {
            panic!("/bin/bash doesn't appear to be a binary yaxpeax understands.");
        }
    };

    // should be x86_64, but we can just try x86_64 anyway and hope for the best
    println!("isa hint: {:?}", isa);

    let mut x86_64_data = yaxpeax_core::arch::x86_64::x86_64Data::default();

    // start queuing up places we expect to find functions
    x86_64_data.contexts.put(*entrypoint as u64, BaseUpdate::Specialized(
        yaxpeax_core::arch::x86_64::x86Update::FunctionHint
    ));

    // copy in symbols (not really necessary here)
    for sym in symbols {
        x86_64_data.contexts.put(sym.addr as u64, BaseUpdate::DefineSymbol(
            Symbol(Library::This, sym.name.clone())
        ));
    }

    // and copy in names for imports
    for import in imports {
        x86_64_data.contexts.put(import.value as u64, BaseUpdate::DefineSymbol(
            Symbol(Library::Unknown, import.name.clone())
        ));
    }

    // exports are probably functions? hopve for the best
    for export in exports {
        x86_64_data.contexts.put(export.addr as u64, BaseUpdate::Specialized(
            yaxpeax_core::arch::x86_64::x86Update::FunctionHint
        ));
    }

    // and recursively traverse all places there might be code, constructing CFGs, DFGs, and doing
    // simple constant propagation
    while let Some(addr) = x86_64_data.contexts.function_hints.pop() {
        println!("analyzing function at {}", addr.show());
        two_iteration_function_analysis(&program, &mut x86_64_data, addr)
    }

    println!("all done, displaying the entrypoint function...");

    let (entry_cfg, entry_dfg) = x86_64_data.ssa.get(entrypoint).expect("just inserted a cfg/dfg pair");

    let colors = ColorSettings::default();

    let entry_display = yaxpeax_core::arch::x86_64::display::show_function(
        &program,
        &x86_64_data.contexts,
        Some(entry_dfg),
        entry_cfg,
        Some(&colors)
    );

    for (_addr, lines) in entry_display.view_between(None, None) {
        for line in lines {
            println!("{}", line);
        }
    }
}

fn function_cfg_for_addr(program: &ModuleData, data: &mut x86_64Data, addr: <AMD64 as Arch>::Address) -> ControlFlowGraph<<AMD64 as Arch>::Address> {
    if !data.contexts.functions.borrow().contains_key(&addr) {
        data.contexts.put(
            addr, BaseUpdate::DefineFunction(
                Function::of(
                    format!("function:{}", addr.show()),
                    vec![],
                    vec![],
                )
            )
        );

        control_flow::explore_all(
            program,
            &mut data.contexts,
            &mut data.cfg,
            vec![addr],
            &yaxpeax_core::arch::x86_64::analyses::all_instruction_analyses
        );
    }
    data.cfg.get_function(addr, &*data.contexts.functions.borrow())
}

fn calculate_dfg(program: &ModuleData, data: &x86_64Data, fn_graph: &ControlFlowGraph<<AMD64 as Arch>::Address>) -> SSA<AMD64> {
    match data.contexts.function_data.get(&fn_graph.entrypoint) {
        Some(aux_data) => {
            static_single_assignment::cytron::generate_ssa::<AMD64, _, _, _, _, _>(
                program,
                fn_graph.entrypoint,
                &data.cfg,
                &fn_graph.graph,
                &*aux_data.borrow(),
                &mut yaxpeax_core::arch::x86_64::analyses::data_flow::NoDisambiguation::default(),
                &*data.contexts.functions.borrow(),
            )
        }
        None => {
            static_single_assignment::cytron::generate_ssa::<AMD64, _, _, _, _, _>(
                program,
                fn_graph.entrypoint,
                &data.cfg,
                &fn_graph.graph,
                &yaxpeax_core::data::modifier::NoModifiers,
                &mut yaxpeax_core::arch::x86_64::analyses::data_flow::NoDisambiguation::default(),
                &*data.contexts.functions.borrow(),
            )
        }
    }
}

fn two_iteration_function_analysis(program: &ModuleData, data: &mut x86_64Data, addr: <AMD64 as Arch>::Address) {
    let function_cfg = function_cfg_for_addr(program, data, addr);
    let dfg = calculate_dfg(program, data, &function_cfg);

    let mut bfs = Bfs::new(&function_cfg.graph, function_cfg.entrypoint);
    while let Some(k) = bfs.next(&function_cfg.graph) {
        let block = function_cfg.get_block(k);
        let mut iter = AMD64::instructions_spanning(program, block.start, block.end);
        while let Some((address, instr)) = iter.next() {
            <AMD64 as ConstEvaluator<AMD64, (), ConcreteDomain>>::evaluate_instruction(&instr, address, &dfg, &(), program);
            <AMD64 as ConstEvaluator<AMD64, (), SymbolicDomain>>::evaluate_instruction(&instr, address, &dfg, &(), program);
            <AMD64 as ConstEvaluator<AMD64, (), ValueSetDomain>>::evaluate_instruction(&instr, address, &dfg, &(), program);
            if ConditionalInference::inferrable_conditional(&instr) {
                let fn_query_ptr = Rc::clone(&data.contexts.functions);
                // TODO: rebuild dfg if this indicates that ssa values are changed
                let aux_data = data.contexts.function_data.entry(function_cfg.entrypoint).or_insert_with(|| RefCell::new(InstructionModifiers::new(fn_query_ptr)));
                ConditionalInference::add_conditional_bounds(block.start, address, &instr, &function_cfg, &dfg, program, &mut aux_data.borrow_mut());
            }
        }
    }

    data.ssa.insert(addr, (function_cfg, dfg));
}

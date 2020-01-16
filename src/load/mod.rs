use crate::can::def::Declaration;
use crate::can::module::{canonicalize_module_defs, Module, ModuleOutput};
use crate::can::scope::Scope;
use crate::can::symbol::Symbol;
use crate::collections::{insert_all, ImMap, SendMap, SendSet};
use crate::constrain::module::constrain_module;
use crate::ident::Ident;
use crate::module::ModuleName;
use crate::parse::ast::{self, Attempting, ExposesEntry, ImportsEntry};
use crate::parse::module::{self, module_defs};
use crate::parse::parser::{Fail, Parser, State};
use crate::region::{Located, Region};
use crate::solve;
use crate::subs::VarStore;
use crate::subs::{Subs, Variable};
use crate::types::{Constraint, Problem};
use bumpalo::Bump;
use futures::future::join_all;
use std::fs::read_to_string;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::spawn_blocking;

#[derive(Debug)]
pub struct Loaded {
    pub requested_module: LoadedModule,
    pub next_var: Variable,
}

#[derive(Debug, Clone)]
struct Env {
    pub src_dir: PathBuf,
}

#[derive(Debug)]
pub enum BuildProblem<'a> {
    FileNotFound(&'a Path),
}

type LoadedDeps = Vec<LoadedModule>;
type DepNames = SendSet<Box<str>>;

#[derive(Clone, Debug, PartialEq)]
pub enum LoadedModule {
    Valid(Module),
    FileProblem {
        filename: PathBuf,
        error: io::ErrorKind,
    },
    ParsingFailed {
        filename: PathBuf,
        fail: Fail,
    },
}

impl LoadedModule {
    pub fn into_module(self) -> Option<Module> {
        match self {
            LoadedModule::Valid(module) => Some(module),
            _ => None,
        }
    }
}

/// The loading process works like this, starting from the given filename (e.g. "main.roc"):
///
/// 1. Open the file.
/// 2. Parse the module's header.
/// 3. For each of its imports, send a message on the channel to the coordinator thread, which
///    will repeat this process to load that module - starting with step 1.
/// 4. Add everything we were able to import unqualified to the module's default scope.
/// 5. Parse the module's defs.
/// 6. Canonicalize the module.
///
/// The loaded_modules argument specifies which modules have already been loaded.
/// It typically contains the standard modules, but is empty when loading the
/// standard modules themselves.
pub async fn load<'a>(
    src_dir: PathBuf,
    filename: PathBuf,
    loaded_deps: &mut LoadedDeps,
    next_var: Variable,
) -> Loaded {
    let env = Env {
        src_dir: src_dir.clone(),
    };
    let (tx, mut rx): (Sender<DepNames>, Receiver<DepNames>) = mpsc::channel(1024);
    let main_tx = tx.clone();
    let arc_var_store = Arc::new(VarStore::new(next_var));
    let var_store = Arc::clone(&arc_var_store);

    // Use spawn_blocking here so that we can proceed to the recv() loop
    // while this is doing blocking work like reading and parsing the file.
    let handle = spawn_blocking(move || load_filename(&env, filename, main_tx, &var_store));
    let requested_module = handle
        .await
        .unwrap_or_else(|err| panic!("Unable to load requested module: {:?}", err));
    let mut all_deps: SendSet<Box<str>> = SendSet::default();

    // Get a fresh env, since the previous one has been consumed
    let env = Env { src_dir };
    // At first, 1 module is pending (namely the `filename` one).
    let mut pending = 1;

    while let Some(module_deps) = rx.recv().await {
        let deps_to_load = module_deps.relative_complement(all_deps.clone());

        // We just loaded 1 module, and gained deps_to_load more
        pending = pending + deps_to_load.len() - 1;

        // Record that these are loaded *before* spawning threads to load them.
        // We don't want to accidentally process them more than once!
        all_deps = all_deps.union(deps_to_load.clone());

        let loaded_modules = join_all(deps_to_load.into_iter().map(|dep| {
            let env = env.clone();
            let tx = tx.clone();
            let var_store = Arc::clone(&arc_var_store);

            // Use spawn_blocking here because we're canonicalizing these in
            // parallel, and canonicalization can potentially block the
            // executor for awhile.
            spawn_blocking(move || load_module(&env, &dep, tx, &var_store))
        }))
        .await;

        for module in loaded_modules {
            loaded_deps.push(module.expect("Unable to load dependent module"));
        }

        // Once we've run out of pending modules to process, we're done!
        if pending == 0 {
            break;
        }
    }

    let next_var: Variable = Arc::try_unwrap(arc_var_store)
        .expect("TODO better error for Arc being unable to unwrap")
        .into();

    Loaded {
        requested_module,
        next_var,
    }
}

fn load_module(
    env: &Env,
    module_name: &str,
    tx: Sender<DepNames>,
    var_store: &VarStore,
) -> LoadedModule {
    let mut filename = PathBuf::new();

    filename.push(env.src_dir.clone());

    // Convert dots in module name to directories
    for part in module_name.split('.') {
        filename.push(part);
    }

    // End with .roc
    filename.set_extension("roc");

    load_filename(env, filename, tx, var_store)
}

fn load_filename(
    env: &Env,
    filename: PathBuf,
    tx: Sender<DepNames>,
    var_store: &VarStore,
) -> LoadedModule {
    match read_to_string(&filename) {
        Ok(src) => {
            let arena = Bump::new();
            // TODO instead of env.arena.alloc(src), we should create a new buffer
            // in the arena as a Vec<'a, u8> and use tokio's AsyncRead::poll_poll_read_buf
            // to read into a `&mut [u8]` like a Vec<'a, u8> instead of using read_to_string.
            // This way, we avoid both heap-allocating the String
            // (which read_to_string does) and also re-allocating it in the arena
            // after read_to_string completes.
            let state = State::new(&src, Attempting::Module);

            // TODO figure out if there's a way to address this clippy error
            // without introducing a borrow error. ("let and return" is literally
            // what the borrow checker suggested using here to fix the problem, so...)
            #[allow(clippy::let_and_return)]
            let answer = match module::module().parse(&arena, state) {
                Ok((ast::Module::Interface { header }, state)) => {
                    let declared_name: Box<str> = header.name.value.as_str().into();

                    // TODO check to see if declared_name is consistent with filename.
                    // If it isn't, report a problem!

                    let mut scope_from_imports = ImMap::default();
                    let mut deps = SendSet::default();

                    for loc_entry in header.imports {
                        deps.insert(load_import(
                            env,
                            loc_entry.region,
                            &loc_entry.value,
                            &mut scope_from_imports,
                        ));
                    }

                    tokio::spawn(async move {
                        let mut tx = tx;

                        // Send the deps to the main thread for processing,
                        // then continue on to parsing and canonicalizing defs.
                        tx.send(deps).await.unwrap();
                    });

                    let symbol_prefix = format!("{}.", declared_name).into();
                    let mut scope = Scope::new(
                        declared_name.clone().into(),
                        symbol_prefix,
                        scope_from_imports,
                    );
                    let (declarations, exposed_imports, constraint) = process_defs(
                        &arena,
                        state,
                        declared_name.clone(),
                        header.exposes.into_iter(),
                        &mut scope,
                        var_store,
                    );
                    let module = Module {
                        name: Some(declared_name),
                        declarations,
                        exposed_imports,
                        constraint,
                    };

                    LoadedModule::Valid(module)
                }
                Ok((ast::Module::App { header }, state)) => {
                    let mut scope_from_imports = ImMap::default();
                    let mut deps = SendSet::default();

                    for loc_entry in header.imports {
                        deps.insert(load_import(
                            env,
                            loc_entry.region,
                            &loc_entry.value,
                            &mut scope_from_imports,
                        ));
                    }

                    tokio::spawn(async move {
                        let mut tx = tx;

                        // Send the deps to the main thread for processing,
                        // then continue on to parsing and canonicalizing defs.
                        tx.send(deps).await.unwrap();
                    });

                    let mut scope = Scope::new(".".into(), ".".into(), scope_from_imports);

                    // The app module has no declared name. Pass it as "".
                    let (declarations, exposed_imports, constraint) = process_defs(
                        &arena,
                        state,
                        "".into(),
                        std::iter::empty(),
                        &mut scope,
                        var_store,
                    );
                    let module = Module {
                        name: None,
                        declarations,
                        exposed_imports,
                        constraint,
                    };

                    LoadedModule::Valid(module)
                }
                Err((fail, _)) => LoadedModule::ParsingFailed { filename, fail },
            };

            answer
        }
        Err(err) => LoadedModule::FileProblem {
            filename,
            error: err.kind(),
        },
    }
}

fn process_defs<'a, I>(
    arena: &'a Bump,
    state: State<'a>,
    home: Box<str>,
    exposes: I,
    scope: &mut Scope,
    var_store: &VarStore,
) -> (Vec<Declaration>, SendMap<Symbol, Variable>, Constraint)
where
    I: Iterator<Item = Located<ExposesEntry<'a>>>,
{
    let (parsed_defs, _) = module_defs()
        .parse(arena, state)
        .expect("TODO gracefully handle parse error on module defs");

    let ModuleOutput {
        declarations,
        exposed_imports,
        lookups,
    } = canonicalize_module_defs(arena, parsed_defs, home.clone(), exposes, scope, var_store);

    let constraint = constrain_module(home.into(), &declarations, lookups);

    (declarations, exposed_imports, constraint)
}

fn load_import(
    env: &Env,
    region: Region,
    entry: &ImportsEntry<'_>,
    scope: &mut ImMap<Ident, (Symbol, Region)>,
) -> Box<str> {
    use crate::parse::ast::ImportsEntry::*;

    match entry {
        Module(module_name, exposes) => {
            for loc_entry in exposes {
                let (key, value) = expose(*module_name, &loc_entry.value, loc_entry.region);

                scope.insert(Ident::Unqualified(key), value);
            }

            module_name.as_str().into()
        }

        SpaceBefore(sub_entry, _) | SpaceAfter(sub_entry, _) => {
            // Ignore spaces.
            load_import(env, region, *sub_entry, scope)
        }
    }
}

fn expose(
    module_name: ModuleName<'_>,
    entry: &ExposesEntry<'_>,
    region: Region,
) -> (Box<str>, (Symbol, Region)) {
    use crate::parse::ast::ExposesEntry::*;

    match entry {
        Ident(ident) => {
            // Since this value is exposed, add it to our module's default scope.
            let symbol = Symbol::from_module(&module_name, ident);

            ((*ident).into(), (symbol, region))
        }
        SpaceBefore(sub_entry, _) | SpaceAfter(sub_entry, _) => {
            // Ignore spaces.
            expose(module_name, *sub_entry, region)
        }
    }
}

pub fn solve_loaded(
    module: &Module,
    problems: &mut Vec<Problem>,
    subs: &mut Subs,
    loaded_deps: LoadedDeps,
) {
    use Declaration::*;
    use LoadedModule::*;

    let mut vars_by_symbol: ImMap<Symbol, Variable> = ImMap::default();
    let mut dep_constraints = Vec::with_capacity(loaded_deps.len());

    // All the exposed imports should be available in the solver's vars_by_symbol
    for (symbol, expr_var) in im::HashMap::clone(&module.exposed_imports) {
        vars_by_symbol.insert(symbol, expr_var);
    }

    // All the top-level defs should also be available in vars_by_symbol
    for decl in &module.declarations {
        match decl {
            Declare(def) => {
                insert_all(&mut vars_by_symbol, def.pattern_vars.clone().into_iter());
            }
            DeclareRec(defs) => {
                for def in defs {
                    insert_all(&mut vars_by_symbol, def.pattern_vars.clone().into_iter());
                }
            }
            InvalidCycle(_, _) => panic!("TODO handle invalid cycles"),
        }
    }

    // Add each loaded module's top-level defs to the Env, so that when we go
    // to solve, looking up qualified idents gets the correct answer.
    //
    // TODO filter these by what's actually exposed; don't add it to the Env
    // unless the other module actually exposes it!
    for loaded_dep in loaded_deps {
        match loaded_dep {
            Valid(valid_dep) => {
                // All deps' exposed imports should also be available
                // in the solver's vars_by_symbol. (The map's keys are
                // fully qualified, so there won't be any collisions
                // with the primary module's exposed imports!)
                for (symbol, expr_var) in valid_dep.exposed_imports {
                    vars_by_symbol.insert(symbol, expr_var);
                }

                // All its top-level defs should also be available in vars_by_symbol
                for decl in valid_dep.declarations {
                    match decl {
                        Declare(def) => {
                            insert_all(&mut vars_by_symbol, def.pattern_vars.into_iter());
                        }

                        DeclareRec(defs) => {
                            for def in defs {
                                insert_all(&mut vars_by_symbol, def.pattern_vars.into_iter());
                            }
                        }

                        InvalidCycle(_, _) => panic!("TODO handle invalid cycles"),
                    }
                }

                let module_name: crate::can::ident::ModuleName = valid_dep.name.unwrap().into();

                dep_constraints.push((module_name, valid_dep.constraint));
            }

            broken @ FileProblem { .. } => {
                panic!("TODO handle FileProblem with loaded dep: {:?}", broken);
            }

            broken @ ParsingFailed { .. } => {
                panic!("TODO handle ParsingFailed with loaded dep: {:?}", broken);
            }
        }
    }

    for (_module_name, dep_constraint) in dep_constraints {
        let subs_by_module = SendMap::default();

        solve::run(
            &vars_by_symbol,
            subs_by_module,
            problems,
            subs,
            &dep_constraint,
        );
    }

    let subs_by_module = SendMap::default();

    solve::run(
        &vars_by_symbol,
        subs_by_module,
        problems,
        subs,
        &module.constraint,
    );
}

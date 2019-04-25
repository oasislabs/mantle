use std::collections::{BTreeMap, BTreeSet}; // BTree for reproducability

use rustc::{hir::intravisit::Visitor, ty::AdtDef};
use rustc_data_structures::sync::Once;

use crate::{
    rpc::RpcInterface,
    visitor::{DefinedTypeCollector, RpcCollector, SyntaxPass},
};

#[derive(Deserialize)]
struct Lockfile {
    package: Vec<LockfileEntry>,
}

#[derive(Deserialize)]
struct LockfileEntry {
    name: String,
    version: String,
}

pub struct IdlGenerator {
    syntax_pass: SyntaxPass,
    iface: Once<RpcInterface>,
    deps: Once<BTreeMap<String, LockfileEntry>>,
}

impl IdlGenerator {
    pub fn new() -> Self {
        Self {
            syntax_pass: SyntaxPass::default(),
            iface: Once::new(),
            deps: Once::new(),
        }
    }

    /// Returns the generated interface.
    /// Only valid after rustc callback has been executed. Panics if called before.
    pub fn get(&self) -> &RpcInterface {
        self.iface.get()
    }

    /// Returns the (name, version) of a dependency.
    fn crate_version<S: AsRef<str>>(&self, crate_name: S) -> String {
        self.deps.init_locking(Self::load_deps);
        let deps = self.deps.get();
        deps.get(crate_name.as_ref())
            .map(|pkg| pkg.version.to_string())
            .unwrap_or_else(|| "*".to_string())
    }

    fn load_deps() -> BTreeMap<String, LockfileEntry> {
        let mf_dir = std::path::PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("`CARGO_MANIFEST_DIR` not set"),
        );

        let lockfile_path = mf_dir
            .ancestors()
            .map(|p| p.join("Cargo.lock"))
            .skip_while(|p| !p.is_file())
            .nth(0);

        if let Some(lockfile_path) = lockfile_path {
            let lockfile: Lockfile = toml::from_str(
                &std::fs::read_to_string(lockfile_path).expect("Cargo.lock should exist"),
            )
            .expect("Cargo.lock should exist and be readable");

            lockfile
                .package
                .into_iter()
                .map(|pkg| (pkg.name.replace("-", "_"), pkg))
                .collect()
        } else {
            BTreeMap::default()
        }
    }
}

impl rustc_driver::Callbacks for IdlGenerator {
    fn after_parsing(&mut self, compiler: &rustc_interface::interface::Compiler) -> bool {
        let parse = compiler
            .parse()
            .expect("`after_parsing` is only called after parsing")
            .peek();
        syntax::visit::walk_crate(&mut self.syntax_pass, &parse);
        true
    }

    fn after_analysis(&mut self, compiler: &rustc_interface::interface::Compiler) -> bool {
        let sess = compiler.session();
        let mut global_ctxt = rustc_driver::abort_on_err(compiler.global_ctxt(), sess).peek_mut();

        let service_name = match self.syntax_pass.service_name() {
            Some(service_name) => service_name,
            None => return true, // `#[contract]` will complain about missing `derive(Contract)`.
        };

        global_ctxt.enter(|tcx| {
            let mut vis = RpcCollector::new(service_name);
            tcx.hir()
                .krate()
                .visit_all_item_likes(&mut vis.as_deep_visitor());

            let defined_types: Vec<&AdtDef> = vis
                .rpcs()
                .iter()
                .flat_map(|(_, decl)| {
                    let mut def_ty_collector = DefinedTypeCollector::new(tcx);
                    def_ty_collector.visit_fn_decl(decl);
                    def_ty_collector.adt_defs()
                })
                .collect();

            let mut imports = BTreeSet::default();
            let mut adt_defs = BTreeSet::default();
            for def_ty in defined_types.into_iter() {
                if def_ty.did.is_local() {
                    adt_defs.insert(def_ty);
                } else {
                    let crate_name = tcx.original_crate_name(def_ty.did.krate);
                    imports.insert((crate_name, self.crate_version(crate_name.as_str())));
                }
            }

            let iface = match RpcInterface::convert(
                tcx,
                service_name,
                imports,
                adt_defs,
                self.syntax_pass.event_indices(),
                vis.rpcs(),
            ) {
                Ok(iface) => iface,
                Err(errs) => {
                    errs.into_iter().for_each(|err| {
                        sess.span_err(err.span(), &format!("{}", err));
                    });
                    return;
                }
            };

            self.iface.set(iface);
        });

        true
    }
}

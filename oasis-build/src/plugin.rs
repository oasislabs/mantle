use std::collections::BTreeSet; // BTree for reproducability

use rustc::{hir::intravisit::Visitor, util::nodemap::FxHashMap};
use rustc_data_structures::sync::Once;
use rustc_driver::Compilation;
use syntax_pos::symbol::Symbol;

use crate::visitor::{
    hir::{AnalyzedRpcCollector, DefinedTypeCollector, EventCollector},
    syntax::{ParsedRpcCollector, ParsedRpcKind, ServiceDefFinder},
};

#[derive(Clone, Debug)]
pub struct BuildContext {
    pub target: BuildTarget,
    pub crate_name: String,
    pub out_dir: std::path::PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildTarget {
    Wasi,
    Test,
    Dep,
}

pub struct BuildPlugin {
    target: BuildTarget,
    imports: FxHashMap<String, String>, // crate_name -> version
    service_name: Once<Symbol>,
    event_indexed_fields: FxHashMap<Symbol, Vec<Symbol>>, // event_name -> field_name
    iface: Once<oasis_rpc::Interface>,
}

impl BuildPlugin {
    pub fn new(target: BuildTarget, imports: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            target,
            imports: imports.into_iter().collect(),
            service_name: Once::new(),
            event_indexed_fields: Default::default(),
            iface: Once::new(),
        }
    }

    /// Returns the generated interface.
    /// Only valid after rustc callback has been executed. Panics if called before.
    pub fn try_get(&self) -> Option<&oasis_rpc::Interface> {
        self.iface.try_get()
    }
}

macro_rules! ret_err {
    () => {{
        std::env::set_var("OASIS_BUILD_NO_SERVICE_DERIVE", "1");
        return Compilation::Continue;
        // ^ Always continue so that compiler catches other errors.
    }};
}

impl rustc_driver::Callbacks for BuildPlugin {
    fn after_parsing(&mut self, compiler: &rustc_interface::interface::Compiler) -> Compilation {
        let gen_dir = compiler
            .output_dir()
            .as_ref()
            .map(std::path::PathBuf::clone)
            .unwrap_or_else(std::env::temp_dir)
            .join("oasis_generated");
        std::fs::create_dir_all(&gen_dir)
            .unwrap_or_else(|_| panic!("Could not create dir: `{}`", gen_dir.display()));

        let crate_name_query = compiler
            .crate_name()
            .expect("Could not determine crate name");
        let crate_name = crate_name_query.take();
        crate_name_query.give(crate_name.clone());

        let sess = compiler.session();
        let mut parse = compiler
            .parse()
            .expect("`after_parsing` is only called after parsing")
            .peek_mut();

        let mut service_def_finder = ServiceDefFinder::default();
        syntax::visit::walk_crate(&mut service_def_finder, &parse);

        let (services, event_indexed_fields) = service_def_finder.get();
        self.event_indexed_fields = event_indexed_fields;

        let main_service = match services.as_slice() {
            [] => return Compilation::Continue, // No services defined. Do nothing.
            [main_service] => main_service,
            _ => {
                sess.span_err(
                    services[1].span,
                    "Multiple invocations of `oasis_std::service!`. Second occurrence here:",
                );
                ret_err!();
            }
        };
        let service_name = main_service.name;
        self.service_name.set(service_name);

        let mut parsed_rpc_collector = ParsedRpcCollector::new(service_name);
        syntax::visit::walk_crate(&mut parsed_rpc_collector, &parse);

        let struct_span = match parsed_rpc_collector.struct_span() {
            Some(s) => s,
            None => {
                sess.span_err(
                    main_service.span,
                    &format!("Could not find state struct for `{}`", service_name),
                );
                ret_err!();
            }
        };

        let (rpcs_result, warnings) = parsed_rpc_collector.into_rpcs();

        for warning in warnings {
            sess.span_warn(warning.span(), &warning.to_string());
        }

        let rpcs = match rpcs_result {
            Ok(rpcs) => rpcs,
            Err(errs) => {
                for err in errs {
                    sess.span_err(err.span(), &format!("{}", err));
                }
                ret_err!();
            }
        };

        let (ctor, rpcs): (Vec<_>, Vec<_>) = rpcs
            .into_iter()
            .partition(|rpc| rpc.kind == ParsedRpcKind::Ctor);
        let ctor_sig = match ctor.as_slice() {
            [] => {
                sess.span_err(
                    struct_span,
                    &format!("Missing definition of `{}::new`.", service_name),
                );
                ret_err!();
            }
            [rpc] => &rpc.sig,
            _ => ret_err!(), // Multiply defined `new` function. Let the compiler catch this.
        };

        let default_fn_spans = rpcs
            .iter()
            .filter_map(|rpc| {
                if let ParsedRpcKind::Default(default_span) = rpc.kind {
                    Some(vec![default_span, rpc.span])
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if default_fn_spans.len() > 1 {
            sess.span_err(
                default_fn_spans
                    .into_iter()
                    .flat_map(std::convert::identity)
                    .collect::<Vec<_>>(),
                "Only one RPC method can be marked with `#[default]`",
            );
            ret_err!();
        }

        let build_context = BuildContext {
            target: self.target,
            crate_name,
            out_dir: gen_dir,
        };
        let service_def = crate::gen::ServiceDefinition {
            name: service_name,
            ctor: &ctor_sig,
            rpcs,
        };

        crate::gen::insert_oasis_bindings(build_context, &mut parse, service_def);

        Compilation::Continue
    }

    fn after_analysis(&mut self, compiler: &rustc_interface::interface::Compiler) -> Compilation {
        let sess = compiler.session();
        let mut global_ctxt = rustc_driver::abort_on_err(compiler.global_ctxt(), sess).peek_mut();

        let service_name = match self.service_name.try_get() {
            Some(service_name) => service_name,
            None => return Compilation::Continue, // No service defined. Do nothing.
        };

        global_ctxt.enter(|tcx| {
            let krate = tcx.hir().krate();
            let mut rpc_collector = AnalyzedRpcCollector::new(krate, tcx, *service_name);
            krate.visit_all_item_likes(&mut rpc_collector);

            let defined_types = rpc_collector.rpcs().iter().flat_map(|(_, decl, _)| {
                let mut def_ty_collector = DefinedTypeCollector::new(tcx);
                def_ty_collector.visit_fn_decl(decl);
                def_ty_collector.adt_defs()
            });

            let mut event_collector = EventCollector::new(tcx);
            tcx.hir()
                .krate()
                .visit_all_item_likes(&mut event_collector.as_deep_visitor());

            let all_adt_defs = defined_types
                .map(|(def, spans)| (def, spans, false /* is_import */))
                .chain(
                    event_collector
                        .adt_defs()
                        .map(|(def, spans)| (def, spans, true)),
                );

            let mut imports = BTreeSet::default();
            let mut adt_defs = BTreeSet::default();
            for (def, spans, is_event) in all_adt_defs {
                if def.did.is_local() {
                    adt_defs.insert((def, is_event));
                } else {
                    let crate_name = tcx.original_crate_name(def.did.krate);
                    match self.imports.get(crate_name.as_str().get()) {
                        Some(version) => {
                            imports.insert((crate_name, version.to_string()));
                        }
                        None => {
                            let err_msg = format!(
                                "External type `{}` must be defined in \
                                 a service to use in an RPC interface.",
                                tcx.def_path_str(def.did)
                            );
                            sess.span_err(spans, &err_msg);
                        }
                    };
                }
            }

            let iface = match crate::rpc::convert_interface(
                tcx,
                *service_name,
                imports,
                adt_defs,
                &self.event_indexed_fields,
                rpc_collector.rpcs(),
            ) {
                Ok(iface) => iface,
                Err(errs) => {
                    for err in errs {
                        sess.span_err(err.span(), &format!("{}", err));
                    }
                    return;
                }
            };

            self.iface.set(iface);
        });

        Compilation::Continue
    }
}

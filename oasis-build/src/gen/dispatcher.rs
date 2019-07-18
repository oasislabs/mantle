use std::path::Path;

use proc_macro2::TokenStream;
use proc_quote::quote;
use syntax::{
    ast::{Arg, Crate, ItemKind, MethodSig, StmtKind},
    print::pprust,
};
use syntax_pos::symbol::Symbol;

use crate::{
    format_ident,
    visitor::syntax::{ParsedRpc, ParsedRpcKind},
    BuildContext,
};

use super::{common, ServiceDefinition};

pub fn insert(build_ctx: &BuildContext, krate: &mut Crate, service_def: &ServiceDefinition) {
    let BuildContext {
        out_dir,
        crate_name,
        ..
    } = build_ctx;

    let ServiceDefinition {
        name: service_name,
        rpcs,
        ctor,
    } = service_def;

    let default_fn = rpcs.iter().find(|rpc| match rpc.kind {
        ParsedRpcKind::Default(_) => true,
        _ => false,
    });

    if !rpcs.is_empty() {
        let rpcs_dispatcher = generate_rpc_dispatcher(*service_name, &rpcs, default_fn);
        let rpcs_include_file = out_dir.join(format!("{}_dispatcher.rs", crate_name));
        common::write_include(&rpcs_include_file, &rpcs_dispatcher.to_string());
        insert_rpc_dispatcher_stub(krate, &rpcs_include_file);
    }

    let ctor_fn = generate_ctor_fn(*service_name, &ctor);
    let ctor_include_file = out_dir.join(format!("{}_ctor.rs", crate_name));
    common::write_include(&ctor_include_file, &ctor_fn.to_string());
    krate
        .module
        .items
        .push(common::gen_include_item(ctor_include_file));
}

fn generate_rpc_dispatcher(
    service_name: Symbol,
    rpcs: &[ParsedRpc],
    default_fn: Option<&ParsedRpc>,
) -> TokenStream {
    let mut any_rpc_returns_result = false;
    let mut rpc_payload_variants = Vec::with_capacity(rpcs.len());
    let rpc_match_arms = rpcs
        .iter()
        .map(|rpc| {
            let (arg_names, arg_tys) = split_args(&rpc.sig.decl.inputs[2..]);

            let rpc_name = format_ident!("{}", rpc.name.as_str().get());
            rpc_payload_variants.push(quote! {
                #rpc_name((#(#arg_tys),*,),)
            });

            if crate::utils::unpack_syntax_ret(&rpc.sig.decl.output).is_result {
                any_rpc_returns_result = true;
                DispatchArm::for_result(rpc.name, &arg_names)
            } else {
                DispatchArm::for_infallible(rpc.name, &arg_names)
            }
            .sunder(rpc.is_mut())
        })
        .collect::<Vec<_>>();

    let default_fn_invocation = if let Some(rpc) = default_fn {
        let default_dispatch = if crate::utils::unpack_syntax_ret(&rpc.sig.decl.output).is_result {
            DispatchArm::for_result(rpc.name, &[])
        } else {
            DispatchArm::for_infallible(rpc.name, &[])
        }
        .sunder(rpc.is_mut())
        .invocation;
        quote! {
            if input.is_empty() {
                #default_dispatch
            }
        }
    } else {
        quote!()
    };

    let output_err_ty = if any_rpc_returns_result {
        quote!(Vec<u8>)
    } else {
        quote!(())
    };

    let err_returner = if any_rpc_returns_result {
        quote!(oasis_std::backend::err(&err_output))
    } else {
        quote!(unreachable!("No RPC function returns Err"))
    };

    let service_ident = format_ident!("{}", service_name.as_str().get());

    quote! {
        #[allow(warnings)]
        fn _oasis_dispatcher() {
            use oasis_std::{Service as _, reexports::serde::Deserialize};

            type Self = #service_ident;

            #[derive(Deserialize)]
            #[serde(tag = "method", content = "payload")]
            enum RpcPayload {
                #(#rpc_payload_variants),*
            }

            let ctx = oasis_std::Context::default(); // TODO(#33)
            let mut service = Self::coalesce();
            let input = oasis_std::backend::input();
            #default_fn_invocation
            let payload: RpcPayload =
                oasis_std::reexports::serde_cbor::from_slice(&input).unwrap();
            let output: std::result::Result<Vec<u8>, #output_err_ty> = match payload {
                #(#rpc_match_arms)*
            };
            match output {
                Ok(output) => oasis_std::backend::ret(&output),
                Err(err_output) => #err_returner,
            }
        }
    }
}

mod armery {
    use super::*;

    pub struct DispatchArm {
        pub guard: TokenStream,
        pub invocation: TokenStream,
        fn_name: proc_macro2::Ident,
        sunder: bool,
    }

    impl DispatchArm {
        fn new_with_guard_only(fn_name: Symbol, arg_names: &[proc_macro2::Ident]) -> Self {
            let fn_name = format_ident!("{}", fn_name);
            Self {
                guard: quote!(RpcPayload::#fn_name((#(#arg_names),*,),)),
                invocation: quote!(),
                sunder: true,
                fn_name,
            }
        }

        pub fn for_result(fn_name: Symbol, arg_names: &[proc_macro2::Ident]) -> Self {
            let mut this = Self::new_with_guard_only(fn_name, arg_names);
            let fn_name = &this.fn_name;
            this.invocation = quote! {
                match service.#fn_name(&ctx, #(#arg_names),*) {
                    Ok(output) => Ok(oasis_std::reexports::serde_cbor::to_vec(&output).unwrap()),
                    Err(err) => Err(oasis_std::reexports::serde_cbor::to_vec(&err).unwrap()),
                }
            };
            this
        }

        pub fn for_infallible(fn_name: Symbol, arg_names: &[proc_macro2::Ident]) -> Self {
            let mut this = Self::new_with_guard_only(fn_name, arg_names);
            let fn_name = &this.fn_name;
            this.invocation = quote! {
                Ok(oasis_std::reexports::serde_cbor::to_vec(
                        &service.#fn_name(&ctx, #(#arg_names),*)
                ).unwrap())
            };
            this
        }

        pub fn sunder(mut self, should_sunder: bool) -> Self {
            self.sunder = should_sunder;
            self
        }
    }

    impl proc_quote::ToTokens for DispatchArm {
        fn to_tokens(&self, tokens: &mut TokenStream) {
            let DispatchArm {
                guard, invocation, ..
            } = self;
            if self.sunder {
                tokens.extend(quote! {
                    #guard => {
                        let output = #invocation;
                        Self::sunder();
                        output
                    }
                });
            } else {
                tokens.extend(quote!(#guard => #invocation));
            }
        }
    }
}
use armery::DispatchArm;

fn generate_ctor_fn(service_name: Symbol, ctor: &MethodSig) -> TokenStream {
    let (arg_names, arg_tys) = split_args(&ctor.decl.inputs[1..]);

    let ctor_payload_unpack = if ctor.decl.inputs.len() > 1 {
        quote! {
            let CtorPayload((#(#arg_names),*),) =
                oasis_std::reexports::serde_cbor::from_slice(&oasis_std::backend::input()).unwrap();
        }
    } else {
        quote!()
    };

    let service_ident = format_ident!("{}", service_name.as_str().get());

    let ctor_stmt = if crate::utils::unpack_syntax_ret(&ctor.decl.output).is_result {
        quote! {
            match <#service_ident>::new(&ctx, #(#arg_names),*) {
                Ok(service) => service,
                Err(err) => {
                    oasis_std::backend::err(&format!("{:#?}", err).into_bytes());
                    return 1;
                }
            }
        }
    } else {
        quote! { <#service_ident>::new(&ctx, #(#arg_names),*) }
    };

    quote! {
        #[allow(warnings)]
        #[no_mangle]
        extern "C" fn _oasis_deploy() -> u8 {
            use oasis_std::{Service as _, reexports::serde::Deserialize};

            #[derive(Deserialize)]
            #[allow(non_camel_case_types)]
            struct CtorPayload((#(#arg_tys),*),);

            let ctx = oasis_std::Context::default(); // TODO(#33)
            #ctor_payload_unpack
            let mut service = #ctor_stmt;
            <#service_ident>::sunder(service);
            return 0;
        }
    }
}

fn insert_rpc_dispatcher_stub(krate: &mut Crate, include_file: &Path) {
    krate
        .module
        .items
        .push(common::gen_include_item(include_file));
    for item in krate.module.items.iter_mut() {
        if item.ident.name != Symbol::intern("main") {
            continue;
        }
        let main_fn_block = match &mut item.node {
            ItemKind::Fn(_, _, _, ref mut block) => block,
            _ => continue,
        };
        let oasis_macro_idx = main_fn_block
            .stmts
            .iter()
            .position(|stmt| match &stmt.node {
                StmtKind::Mac(mac) => {
                    let mac_ = &mac.0.node;
                    crate::utils::path_ends_with(&mac_.path, &["oasis_std", "service"])
                }
                _ => false,
            })
            .unwrap();
        main_fn_block.stmts.splice(
            oasis_macro_idx..=oasis_macro_idx,
            std::iter::once(common::gen_call_stmt(
                syntax::source_map::symbol::Ident::from_str("_oasis_dispatcher"),
            )),
        );
        break;
    }
}

/// Returns (arg_names, arg_tys)
fn split_args(args: &[Arg]) -> (Vec<proc_macro2::Ident>, Vec<syn::Type>) {
    args.iter()
        .map(|arg| {
            (
                match arg.pat.node {
                    syntax::ast::PatKind::Ident(_, ident, _) => format_ident!("{}", ident),
                    _ => unreachable!("Checked during visitation."),
                },
                syn::parse_str::<syn::Type>(&pprust::ty_to_string(&arg.ty)).unwrap(),
            )
        })
        .unzip()
}

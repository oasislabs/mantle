#[proc_macro]
pub fn contract(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let contract_def = parse_macro_input!(input as syn::File);
    let def_span = contract_def.span().unwrap(); // save this for error reporting later

    let mut contracts: Vec<syn::ItemStruct> = Vec::new();
    let mut other_items: Vec<syn::Item> = Vec::new();
    for item in contract_def.items.into_iter() {
        match item {
            syn::Item::Struct(s) if has_derive(&s, "Contract") => {
                contracts.push(s);
            }
            _ => other_items.push(item),
        };
    }

    if contracts.is_empty() {
        def_span
            .error("Contract definition must contain a #[derive(Contract)] struct.")
            .emit();
    } else if contracts.len() > 1 {
        err!(
            contracts[1],
            "Contract definition must contain exactly one #[derive(Contract)] struct. Second occurrence here:"
        );
    }
    let contract = match contracts.into_iter().nth(0) {
        Some(contract) => contract,
        None => {
            return proc_macro::TokenStream::from(quote! {
                #(#other_items)*
            });
        }
    };
    let contract_name = &contract.ident;

    // transform `lazy!(val)` into `Lazy::_new(key, val)`
    other_items.iter_mut().for_each(|item| {
        LazyInserter {}.visit_item_mut(item);
    });

    let (ctor, rpcs): (Vec<RPC>, Vec<RPC>) = other_items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(imp) if is_impl_of(&imp, contract_name) => Some(imp),
            _ => None,
        })
        .flat_map(|imp| {
            imp.items.iter().filter_map(move |item| match item {
                syn::ImplItem::Method(
                    m @ syn::ImplItemMethod {
                        vis: syn::Visibility::Public(_),
                        ..
                    },
                ) => Some(RPC::new(imp, m)),
                _ => None,
            })
        })
        .partition(|rpc| rpc.ident == "new");

    let ctor = ctor.into_iter().nth(0);

    let rpc_defs: Vec<proc_macro2::TokenStream> = rpcs
        .iter()
        .map(|rpc| {
            let ident = rpc.ident;
            let inps = rpc.structify_inps();
            // e.g., `my_method { my_input: String, my_other_input: u64 }`
            quote! {
                #ident { #(#inps),* }
            }
        })
        .collect();

    // generate match arms to statically dispatch RPCs based on deserialized payload
    let call_tree: Vec<proc_macro2::TokenStream> = rpcs
        .iter()
        .map(|rpc| {
            let ident = rpc.ident;
            let arg_names = rpc.input_names();
            let call_names = arg_names.clone();
            quote! {
                RPC::#ident { #(#arg_names),* } => {
                    serde_cbor::to_vec(&contract.#ident(Context {}, #(#call_names),*))
                }
            }
        })
        .collect();

    let (ctor_inps, ctor_args) = ctor
        .map(|ctor| (ctor.structify_inps(), ctor.input_names()))
        .unwrap_or((Vec::new(), Vec::new()));
    let deploy_payload = if ctor_inps.is_empty() {
        quote! {}
    } else {
        quote! {
            let payload: Ctor = serde_cbor::from_slice(&oasis::input()).unwrap();
        }
    };

    let deploy_mod_ident =
        syn::Ident::new(&format!("_deploy_{}", contract_name), contract_name.span());
    proc_macro::TokenStream::from(quote! {
        #[macro_use]
        extern crate oasis_std;

        use oasis_std::prelude::*;

        #contract

        #(#other_items)*

        #[cfg(feature = "deploy")]
        #[allow(non_snake_case)]
        mod #deploy_mod_ident {
            use super::*;

            #[derive(serde::Serialize, serde::Deserialize)]
            #[serde(tag = "method", content = "payload")]
            #[allow(non_camel_case_types)]
            enum RPC {
                #(#rpc_defs),*
            }

            #[no_mangle]
            fn call() {
                let mut contract = <#contract_name>::coalesce();
                let payload: RPC = serde_cbor::from_slice(&oasis::input()).unwrap();
                let result = match payload {
                    #(#call_tree),*
                }.unwrap();
                OLinks::sunder(contract);
                oasis::ret(&result);
            }

            struct Ctor {
                #(#ctor_inps),*
            }

            #[no_mangle]
            pub fn deploy() {
                #deploy_payload
                #contract_name::sunder(#contract_name::new(Context {}, #(payload.#ctor_args),*));
            }
        }
    })
}

struct LazyInserter {}
impl syn::visit_mut::VisitMut for LazyInserter {
    fn visit_field_value_mut(&mut self, fv: &mut syn::FieldValue) {
        match fv.expr {
            syn::Expr::Macro(ref m) if m.mac.path.is_ident("lazy") => {
                let key = match fv.member {
                    syn::Member::Named(ref ident) => keccak_key(ident),
                    syn::Member::Unnamed(syn::Index { index, .. }) => quote! { H256::from(#index) },
                };
                let val = &m.mac.tts;
                fv.expr = parse_quote!(Lazy::_new(H256::from(#key), #val));
            }
            _ => (),
        }
        syn::visit_mut::visit_field_value_mut(self, fv);
    }
}

struct RPC<'a> {
    ident: &'a syn::Ident,
    inputs: Vec<(&'a syn::Pat, &'a syn::Type)>,
}

impl<'a> RPC<'a> {
    fn new(imp: &'a syn::ItemImpl, m: &'a syn::ImplItemMethod) -> Self {
        let sig = &m.sig;
        let ident = &sig.ident;
        if let Some(abi) = &sig.abi {
            err!(abi, "RPC methods cannot declare an ABI.");
        }
        if let Some(unsafe_) = sig.unsafety {
            err!(unsafe_, "RPC methods may not be unsafe.");
        }
        let decl = &sig.decl;
        if decl.generics.type_params().count() > 0 {
            err!(
                decl.generics,
                "RPC methods may not have generic type parameters.",
            );
        }
        if let Some(variadic) = decl.variadic {
            err!(variadic, "RPC methods may not be variadic.");
        }

        let typ = &*imp.self_ty;
        let mut inps = decl.inputs.iter().peekable();
        if ident == "new" {
            check_next_arg!(
                decl,
                inps,
                RPC::is_context,
                format!(
                    "`{}::new` must take `Context` as its first argument",
                    quote!(#typ)
                )
            );
            match &decl.output {
                syn::ReturnType::Type(_, t) if &**t == typ || t == &parse_quote!(Self) => (),
                ret => {
                    err!(ret, "`{}::new` must return `Self`", quote!(#typ));
                }
            }
            Self {
                ident,
                inputs: inps.filter_map(RPC::check_arg).collect(),
            }
        } else {
            check_next_arg!(
                decl,
                inps,
                RPC::is_self_ref,
                format!(
                    "First argument to `{}::{}` should be &[mut ]self.",
                    quote!(#typ),
                    quote!(ident)
                )
            );
            check_next_arg!(
                decl,
                inps,
                RPC::is_context,
                format!(
                    "Second argument to `{}::{}` should be &Context.",
                    quote!(#typ),
                    quote!(ident)
                )
            );
            Self {
                ident,
                inputs: inps.filter_map(RPC::check_arg).collect(),
            }
        }
    }

    fn is_context(arg: &syn::FnArg) -> bool {
        match arg {
            syn::FnArg::Captured(syn::ArgCaptured { ty, .. })
                if ty == &parse_quote!(Context) || ty == &parse_quote!(oasis_std::Context) =>
            {
                true
            }
            _ => false,
        }
    }

    fn is_self_ref(arg: &syn::FnArg) -> bool {
        match arg {
            syn::FnArg::SelfRef(_) => true,
            _ => false,
        }
    }

    fn check_arg(arg: &syn::FnArg) -> Option<(&syn::Pat, &syn::Type)> {
        match arg {
            syn::FnArg::Captured(syn::ArgCaptured { pat, ty, .. }) => Some((pat, ty)),
            syn::FnArg::Ignored(_) => {
                err!(arg, "Arguments to RPCs must have explicit names.");
                None
            }
            syn::FnArg::Inferred(_) => {
                err!(arg, "Arguments to RPCs must have explicit types.");
                None
            }
            _ => None,
        }
    }

    fn structify_inps(&self) -> Vec<proc_macro2::TokenStream> {
        self.inputs
            .iter()
            .map(|(name, ty)| quote!( #name: #ty ))
            .collect()
    }

    fn input_names(&self) -> Vec<proc_macro2::TokenStream> {
        self.inputs
            .iter()
            .map(|(name, _ty)| quote!( #name ))
            .collect()
    }
}

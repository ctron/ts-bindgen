extern crate proc_macro;

mod ir;
mod parse;
mod module_resolution;

// TODO: aliases should point to modules
// TODO: when generating code, use include_str! to make the compiler think we have a dependency on
// any ts files we use so we recompile when they do:
// https://github.com/rustwasm/wasm-bindgen/pull/1295/commits/b762948456617ee263de8e43b3636bd3a4d1da75

use proc_macro::TokenStream;
use proc_macro2::{TokenStream as TokenStream2};
use quote::{quote, format_ident, ToTokens, TokenStreamExt};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf, Component};
use std::convert::{From, Into};
use std::rc::Rc;
use std::cell::RefCell;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{parse_macro_input, LitStr, Result as ParseResult, Token, parse_str as parse_syn_str};
use unicode_xid::UnicodeXID;
use heck::{SnakeCase, CamelCase};
use ir::{Type, TypeInfo, NamespaceImport, Indexer, Member, Func, Param, EnumMember, TypeName, TypeIdent};
use parse::TsTypes;

#[proc_macro]
pub fn import_ts(input: TokenStream) -> TokenStream {
    let import_args = parse_macro_input!(input as ImportArgs);
    let mods = import_args
        .modules
        .iter()
        .map(|module| {
            let tt = TsTypes::try_new(&module).expect("tt error");
            use std::borrow::Borrow;
            let mod_def: ModDef = tt.types_by_name_by_file.borrow().into();
            let mod_toks = quote! { #mod_def };
            // let mod_toks = quote! { };

            let mut file = std::fs::File::create("output.rs").expect("failed to create file");
            std::io::Write::write_all(&mut file, mod_toks.to_string().as_bytes()).expect("failed to write");

            mod_toks
        })
        .collect::<Vec<TokenStream2>>();
    (quote! {
        #(#mods)*
    })
    .into()
}

struct ImportArgs {
    modules: Vec<String>,
}

impl Parse for ImportArgs {
    fn parse(input: ParseStream) -> ParseResult<Self> {
        let modules = Punctuated::<LitStr, Token![,]>::parse_terminated(input)?;
        Ok(ImportArgs {
            modules: modules.into_iter().map(|m| m.value()).collect(),
        })
    }
}

#[derive(Debug, Clone)]
struct MutModDef {
    name: proc_macro2::Ident,
    types: Vec<Type>,
    children: Vec<Rc<RefCell<MutModDef>>>,
}

impl MutModDef {
    fn to_mod_def(self) -> ModDef {
        ModDef {
            name: self.name,
            types: self.types,
            children: self.children.into_iter().map(move |c| Rc::try_unwrap(c).expect("Rc still borrowed").into_inner().to_mod_def()).collect(),
        }
    }

    fn add_child_mod(&mut self, mod_name: proc_macro2::Ident, types: Vec<Type>) -> Rc<RefCell<MutModDef>> {
        if let Some(child) = self
            .children
            .iter()
            .find(|c| c.borrow().name == mod_name) {
            let child = child.clone();
            child.borrow_mut().types.extend(types);
            child
        } else {
            let child = Rc::new(RefCell::new(MutModDef {
                name: mod_name,
                types ,
                children: Default::default()
            }));
            self.children.push(child.clone());
            child
        }
    }
}

#[derive(Debug, Clone)]
struct ModDef {
    name: proc_macro2::Ident,
    types: Vec<Type>,
    children: Vec<ModDef>,
}

trait ToModPathIter {
    fn to_mod_path_iter(&self) -> Box<dyn Iterator<Item = proc_macro2::Ident>>;
}

impl ToModPathIter for Path {
    fn to_mod_path_iter(&self) -> Box<dyn Iterator<Item = proc_macro2::Ident>> {
        Box::new(
            self
                .canonicalize()
                .expect("canonicalize failed")
                .components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy()),
                    _ => None,
                })
                .rev()
                .take_while(|p| p != "node_modules")
                .map(|p| p.as_ref().to_string())
                .collect::<Vec<String>>()
                .into_iter()
                .rev()
                .map(|n| to_ns_name(&n))
        )
    }
}

impl ToModPathIter for TypeIdent {
    fn to_mod_path_iter(&self) -> Box<dyn Iterator<Item = proc_macro2::Ident>> {
        if let TypeIdent::QualifiedName(names) = &self {
            Box::new(
                (&names[..names.len() - 1]).to_vec().into_iter().map(|n| to_snake_case_ident(&n))
            )
        } else {
            Box::new(vec![].into_iter())
        }
    }
}

impl ToModPathIter for TypeName {
    fn to_mod_path_iter(&self) -> Box<dyn Iterator<Item = proc_macro2::Ident>> {
        Box::new(
            self.file.to_mod_path_iter().chain(self.name.to_mod_path_iter())
        )
    }
}

// TODO: maybe don't make "index" namespaces and put their types in the parent
impl From<&HashMap<PathBuf, HashMap<TypeIdent, Type>>> for ModDef {
    fn from(types_by_name_by_file: &HashMap<PathBuf, HashMap<TypeIdent, Type>>) -> Self {
        let root = Rc::new(RefCell::new(MutModDef {
            name: to_ns_name("root"),
            types: Default::default(),
            children: Default::default()
        }));

        types_by_name_by_file.iter().for_each(|(path, types_by_name)| {
            // given a path like /.../node_modules/a/b/c, we fold over
            // [a, b, c].
            // given a path like /a/b/c (without a node_modules), we fold
            // over [a, b, c].
            let mod_path = path.to_mod_path_iter().collect::<Vec<proc_macro2::Ident>>();
            let last_idx = mod_path.len() - 1;

            mod_path
                .iter()
                .enumerate()
                .fold(
                    root.clone(),
                    move |parent, (i, mod_name)| {
                        let mut parent = parent.borrow_mut();
                        let types = if i == last_idx {
                            types_by_name.values().cloned().collect::<Vec<Type>>()
                        } else {
                            Default::default()
                        };
                        parent.add_child_mod(mod_name.clone(), types)
                    }
                );

            types_by_name
                .iter()
                .filter_map(|(name, typ)| {
                    if let TypeIdent::QualifiedName(names) = name {
                        Some((name.to_mod_path_iter().collect::<Vec<proc_macro2::Ident>>(), typ))
                    } else {
                        None
                    }
                }).for_each(|(names, typ)| {
                    let last_idx = mod_path.len() + names.len() - 1;
                    mod_path
                        .iter()
                        .chain(names.iter())
                        .enumerate()
                        .fold(
                            root.clone(),
                            move |parent, (i, mod_name)| {
                                let mut parent = parent.borrow_mut();
                                let types = if i == last_idx {
                                    vec![typ.clone()]
                                } else {
                                    Default::default()
                                };
                                parent.add_child_mod(mod_name.clone(), types)
                            }
                        );
                });
        });

        Rc::try_unwrap(root).unwrap().into_inner().to_mod_def()
    }
}

fn to_ident(s: &str) -> proc_macro2::Ident {
    // make sure we have valid characters
    let mut chars = s.chars();
    let first: String = chars.by_ref().take(1).map(|first| {
        if UnicodeXID::is_xid_start(first) && first != '_' {
            first.to_string()
        } else {
            "".to_string()
        }
    }).collect();

    let rest: String = chars.map(|c| {
        if UnicodeXID::is_xid_continue(c) {
            c
        } else {
            '_'
        }
    }).collect();

    // now, make sure we have a valid rust identifier (no keyword collissions)
    let mut full_ident = first + &rest;
    while parse_syn_str::<syn::Ident>(&full_ident).is_err() {
        full_ident += "_";
    }

    format_ident!("{}", &full_ident)
}

fn to_camel_case_ident(s: &str) -> proc_macro2::Ident {
    let s = s.to_camel_case();
    to_ident(&s)
}

fn to_ns_name(ns: &str) -> proc_macro2::Ident {
    let ns = ns.to_snake_case();
    to_ident(ns.trim_end_matches(".d.ts").trim_end_matches(".ts"))
}

fn to_snake_case_ident(s: &str) -> proc_macro2::Ident {
    let s = s.to_snake_case();
    to_ident(&s)
}

impl ToTokens for ModDef {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let mod_name = &self.name;
        let types = &self.types;
        let children = &self.children;

        // TODO: would be nice to do something like use super::super::... as ts_bindgen_root and be
        // able to refer to it in future use clauses. just need to get the nesting level here
        let our_toks = quote! {
            #[cfg(target_arch = "wasm32")]
            pub mod #mod_name {
                use wasm_bindgen::prelude::*;

                #(#types)*

                #(#children)*
            }
        };

        toks.append_all(our_toks);
    }
}

impl ToTokens for EnumMember {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let id = to_camel_case_ident(&self.id);
        let our_toks = {
            if let Some(value) = &self.value {
                quote! {
                    #id = #value
                }
            } else {
                quote! {
                    #id
                }
            }
        };
        toks.append_all(our_toks);
    }
}

trait ToNsPath<T: ?Sized> {
    // TODO: would love to return a generic ToTokens...
    fn to_ns_path(&self, current_mod: &T) -> TokenStream2;
}

impl<T, U> ToNsPath<T> for U where T: ToModPathIter, U: ToModPathIter + ?Sized {
    fn to_ns_path(&self, current_mod: &T) -> TokenStream2 {
        let ns_len = current_mod.to_mod_path_iter().count();
        let mut use_path = vec![format_ident!("super"); ns_len];
        use_path.extend(
            self.to_mod_path_iter()
        );
        quote! {
            #(#use_path)::*
        }
    }
}

fn to_unique_ident(mut desired: String, taken: &Fn(&str) -> bool) -> proc_macro2::Ident {
    while taken(&desired) {
        desired += "_";
    }

    to_ident(&desired)
}

impl ToTokens for Type {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let js_name = self.name.to_name();
        let name = to_camel_case_ident(&js_name);

        let our_toks = match &self.info {
            TypeInfo::Interface {
                indexer,
                fields,
            } => {
                let mut field_toks = fields.iter().map(|(js_field_name, typ)| {
                    let field_name = to_snake_case_ident(js_field_name);
                    quote! {
                        #[serde(rename = #js_field_name)]
                        pub #field_name: #typ
                    }
                }).collect::<Vec<TokenStream2>>();

                if let Some(Indexer { readonly, type_info }) = &indexer {
                    let extra_fields_name = to_unique_ident("extra_fields".to_string(), &|x| fields.contains_key(x));

                    field_toks.push(
                        quote! {
                            #[serde(flatten)]
                            pub #extra_fields_name: std::collections::HashMap<String, #type_info>
                        }
                    );
                }

                quote! {
                    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
                    pub struct #name {
                        #(#field_toks),*
                    }
                }
            },
            TypeInfo::Enum { members, } => {
                quote! {
                    #[wasm_bindgen]
                    #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
                    pub enum #name {
                        #(#members),*
                    }
                }
            },
            TypeInfo::Ref { .. } => panic!("ref isn't a top-level type"),
            TypeInfo::Alias { target } => {
                // we super::super our way up to root and then append the target namespace
                let use_path = target.to_ns_path(&self.name);

                quote! {
                    use #use_path as #name;
                }
            },
            TypeInfo::PrimitiveAny {} => {
                quote! {
                    pub type #name = JsValue;
                }
            },
            TypeInfo::PrimitiveNumber {} => {
                quote! {
                    pub type #name = f64;
                }
            },
            TypeInfo::PrimitiveObject {} => {
                quote! {
                    pub type #name = std::collections::HashMap<String, JsValue>;
                }
            },
            TypeInfo::PrimitiveBoolean {} => {
                quote! {
                    pub type #name = bool;
                }
            },
            TypeInfo::PrimitiveBigInt {} => {
                // TODO
                quote! {
                    pub type #name = u64;
                }
            },
            TypeInfo::PrimitiveString {} => {
                quote! {
                    pub type #name = String;
                }
            },
            TypeInfo::PrimitiveSymbol {} => panic!("how do we handle symbols"),
            TypeInfo::PrimitiveVoid {} => {
                quote! {}
            },
            TypeInfo::PrimitiveUndefined {} => {
                quote! {}
            },
            /*
            TypeInfo::PrimitiveNull {},
            TypeInfo::BuiltinPromise {
                value_type: Box<TypeInfo>,
            },
            TypeInfo::BuiltinDate {},
            TypeInfo::Array {
                item_type: Box<TypeInfo>,
            },
            TypeInfo::Optional {
                item_type: Box<TypeInfo>,
            },
            TypeInfo::Union {
                types: Vec<TypeInfo>,
            },
            TypeInfo::Intersection {
                types: Vec<TypeInfo>,
            },
            TypeInfo::Mapped {
                value_type: Box<TypeInfo>,
            },
            TypeInfo::LitNumber {
                n: f64,
            },
            TypeInfo::LitString {
                s: String,
            },
            TypeInfo::LitBoolean {
                b: bool,
            },*/
            TypeInfo::Func(Func { params, type_params, return_type }) => {
                let fn_name = to_snake_case_ident(&js_name);
                let mut is_variadic = false;
                let param_toks: Vec<TokenStream2> = params.iter().map(|p| {
                    let param_name = to_snake_case_ident(&p.name);
                    let typ = &p.type_info;
                    let full_type = if p.is_variadic {
                        is_variadic = true;
                        quote! {
                            &[#typ]
                        }
                    } else {
                        quote! {
                            #typ
                        }
                    };

                    quote! {
                        #param_name: #full_type
                    }
                }).collect();

                let attrs = {
                    let mut attrs = vec![quote! { js_name = #js_name }];
                    if is_variadic {
                        attrs.push(quote! { variadic });
                    }
                    attrs
                };

                quote! {
                    #[wasm_bindgen]
                    extern "C" {
                        #[wasm_bindgen(#(#attrs),*)]
                        fn #fn_name(#(#param_toks),*) -> #return_type;
                    }
                }
            },
            /*
            TypeInfo::Constructor {
                params: Vec<Param>,
                return_type: Box<TypeInfo>,
            },
            TypeInfo::Class {
                members: HashMap<String, Member>,
            },
            TypeInfo::Var {
                type_info: Box<TypeInfo>,
            },
            TypeInfo::GenericType {
                name: String,
                constraint: Box<TypeInfo>,
            },*/
            TypeInfo::NamespaceImport(NamespaceImport::All { src }) => {
                let ns = src.as_path().to_ns_path(&self.name);
                let vis = if self.is_exported {
                    let vis = format_ident!("pub");
                    quote! { #vis }
                } else {
                    quote! {}
                };
                let name = to_snake_case_ident(&js_name);

                quote! {
                    #vis use #ns as #name;
                }
            },
            TypeInfo::NamespaceImport(NamespaceImport::Default { src }) => {
                let ns = src.as_path().to_ns_path(&self.name);
                let vis = if self.is_exported {
                    let vis = format_ident!("pub");
                    quote! { #vis }
                } else {
                    quote! {}
                };
                let default_export = to_ident("default");

                quote! {
                    #vis use #ns::#default_export as #name;
                }
            },
            TypeInfo::NamespaceImport(NamespaceImport::Named { src, name: item_name }) => {
                let ns = src.as_path().to_ns_path(&self.name);
                let vis = if self.is_exported {
                    let vis = format_ident!("pub");
                    quote! { #vis }
                } else {
                    quote! {}
                };
                let item_name = to_camel_case_ident(item_name);

                quote! {
                    #vis use #ns::#item_name as #name;
                }
            },
            _ => { quote! { }},
        };

        toks.append_all(our_toks);
    }
}

impl ToTokens for TypeInfo {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let our_toks = match &self {
            TypeInfo::Interface { .. } => {
                panic!("interface in type info");
            },
            TypeInfo::Enum { .. } => {
                panic!("enum in type info");
            },
            TypeInfo::Ref { referent, type_params } =>  {
                let local_name = to_camel_case_ident(&referent.to_name());

                quote! {
                    #local_name
                }
            },
            TypeInfo::Alias { target } => {
                // TODO: need to get the local name for the alias (stored on the Type right now)
                let local_name = to_camel_case_ident(&target.to_name());

                quote! {
                    #local_name
                }
            },
            TypeInfo::PrimitiveAny {} => {
                quote! {
                    JsValue
                }
            },
            TypeInfo::PrimitiveNumber {} => {
                quote! {
                    f64
                }
            },
            TypeInfo::PrimitiveObject {} => {
                quote! {
                    std::collections::HashMap<String, JsValue>
                }
            },
            TypeInfo::PrimitiveBoolean {} => {
                quote! {
                    bool
                }
            },
            TypeInfo::PrimitiveBigInt {} => {
                // TODO
                quote! {
                    u64
                }
            },
            TypeInfo::PrimitiveString {} => {
                quote! {
                    String
                }
            },
            TypeInfo::PrimitiveSymbol {} => panic!("how do we handle symbols"),
            TypeInfo::PrimitiveVoid {} => {
                quote! {
                    ()
                }
            },
            TypeInfo::PrimitiveUndefined {} => {
                // TODO
                quote! {}
            },
            TypeInfo::PrimitiveNull {} => {
                // TODO
                quote! {}
            },
            TypeInfo::BuiltinPromise { value_type } => {
                // TODO: should be an async function with Result return type
                quote! {
                    js_sys::Promise
                }
            },
            TypeInfo::BuiltinDate {} => {
                // TODO
                quote! {
                    js_sys::Date
                }
            },
            TypeInfo::Array { item_type } => {
                quote! {
                    Vec<#item_type>
                }
            },
            TypeInfo::Optional { item_type } => {
                quote! {
                    Option<#item_type>
                }
            },
            TypeInfo::Union { types } => {
                // TODO
                quote! {}
            },
            TypeInfo::Intersection { types } => {
                // TODO
                quote! {}
            },
            TypeInfo::Mapped { value_type } => {
                quote! {
                    std::collections::HashMap<String, #value_type>
                }
            },
            TypeInfo::LitNumber { n } => {
                // TODO
                quote! {
                    f64
                }
            },
            TypeInfo::LitString { s } => {
                // TODO
                quote! {
                    String
                }
            },
            TypeInfo::LitBoolean { b } => {
                // TODO
                quote! {
                    bool
                }
            },
            TypeInfo::Func(Func { params, type_params, return_type }) => {
                let param_toks: Vec<TokenStream2> = params.iter().map(|p| {
                    let param_name = to_snake_case_ident(&p.name);
                    let typ = &p.type_info;

                    if p.is_variadic {
                        quote! {
                            &[#typ]
                        }
                    } else {
                        quote! {
                            #typ
                        }
                    }
                }).collect();

                quote! {
                    &Closure<dyn Fn(#(#param_toks),*) -> #return_type>
                }
            },
            /*
            TypeInfo::Constructor {
                params: Vec<Param>,
                return_type: Box<TypeInfo>,
            },
            TypeInfo::Class {
                members: HashMap<String, Member>,
            },
            TypeInfo::Var {
                type_info: Box<TypeInfo>,
            },
            TypeInfo::GenericType {
                name: String,
                constraint: Box<TypeInfo>,
            },*/
            TypeInfo::NamespaceImport(_) => panic!("namespace import in type info"),
            _ => { quote! { }},
        };

        toks.append_all(our_toks);
    }
}

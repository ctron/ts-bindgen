mod funcs;
mod generics;
mod named;
mod resolve_target_type;
mod serialization_type;
mod traits;
mod type_ref_like;

use crate::codegen::funcs::{
    fn_types, render_raw_return_to_js, Constructor, FnPrototypeExt, HasFnPrototype, InternalFunc,
    WrapperFunc,
};
use crate::codegen::generics::{apply_type_params, render_type_params, ResolveGeneric};
use crate::codegen::named::Named;
use crate::codegen::resolve_target_type::ResolveTargetType;
use crate::codegen::serialization_type::{SerializationType, SerializationTypeGetter};
use crate::codegen::traits::render_trait_defn;
use crate::codegen::type_ref_like::OwnedTypeRef;
use crate::identifier::{
    to_camel_case_ident, to_ident, to_snake_case_ident, to_unique_ident, Identifier,
};
use crate::ir::{
    Alias, Builtin, Class, Context, Enum, EnumMember, Func, Indexer, Interface, Intersection,
    Member, NamespaceImport, TargetEnrichedType, TargetEnrichedTypeInfo, Tuple, TypeIdent,
    TypeParamConfig, TypeRef, Union,
};
pub use crate::mod_def::ModDef;
use crate::mod_def::ToModPathIter;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, ToTokens, TokenStreamExt};
use std::borrow::Cow;
use std::collections::HashMap;
use std::iter;
use std::path::{Path, PathBuf};
use syn::Token;

macro_rules! trait_impl_for_type_info {
    (match $matcher:ident,
     $invoker:path | $default:tt,
     $($case:pat => $res:expr),* $(,)?) => {
        match $matcher {
            $($case => $res),*,
            #[allow(unreachable_patterns)]
            TargetEnrichedTypeInfo::Alias(a) => a
                .resolve_target_type()
                .as_ref()
                .map($invoker)
                .unwrap_or($default),
            #[allow(unreachable_patterns)]
            TargetEnrichedTypeInfo::Ref(r) => match &r.referent {
                TypeIdent::GeneratedName { .. } => unreachable!(),
                _ => r
                    .resolve_target_type()
                    .as_ref()
                    .map($invoker)
                    .unwrap_or($default),
            },
            #[allow(unreachable_patterns)]
            TargetEnrichedTypeInfo::Optional { item_type } => $invoker(item_type.as_ref()),
            #[allow(unreachable_patterns)]
            TargetEnrichedTypeInfo::Array { item_type } => $invoker(item_type.as_ref()),
            #[allow(unreachable_patterns)]
            TargetEnrichedTypeInfo::Mapped { value_type } => $invoker(value_type.as_ref()),
            #[allow(unreachable_patterns)]
            TargetEnrichedTypeInfo::NamespaceImport(n) => n
                .resolve_target_type()
                .as_ref()
                .map($invoker)
                .unwrap_or($default),
        }
    };
    (match $matcher:ident,
     $invoker:path | $default:tt,
     aggregate with $agg:ident,
     $($case:pat => $res:expr),* $(,)?) => {
        trait_impl_for_type_info!(
            match $matcher,
            $invoker | $default,
            TargetEnrichedTypeInfo::Interface(i) => i.fields.values()
                .map(ResolveTargetType::resolve_target_type)
                .chain(iter::once(
                    i.indexer.as_ref()
                        .and_then(|i| i.value_type.resolve_target_type())
                ))
                .$agg(|t| t.as_ref().map($invoker).unwrap_or($default)),
            TargetEnrichedTypeInfo::Union(Union { types, .. }) => types.iter().$agg($invoker),
            TargetEnrichedTypeInfo::Intersection(Intersection { types, .. }) => types.iter().$agg($invoker),
            TargetEnrichedTypeInfo::Tuple(Tuple { types, .. }) => types.iter().$agg($invoker),
            $($case => $res),*
        )
    };
}

impl ToTokens for Identifier {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        toks.append_separated(self.type_parts.iter(), <Token![::]>::default());
        if !self.type_params.is_empty() {
            toks.extend(iter::once("<".parse::<TokenStream2>().unwrap()));
            let mut type_params = self.type_params.iter();
            if let Some(tp) = type_params.next() {
                tp.to_tokens(toks);
            }
            type_params.for_each(|tp| {
                toks.extend(iter::once(",".parse::<TokenStream2>().unwrap()));
                tp.to_tokens(toks);
            });
            toks.extend(iter::once(">".parse::<TokenStream2>().unwrap()));
        }
    }
}

impl ToTokens for ModDef {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let mod_name = &self.name;
        let types = &self.types;
        let children = &self.children;

        let imports = if types.is_empty() {
            quote! {}
        } else {
            quote! {
                use wasm_bindgen::prelude::*;
            }
        };

        // TODO: would be nice to do something like use super::super::... as ts_bindgen_root and be
        // able to refer to it in future use clauses. just need to get the nesting level here
        let our_toks = quote! {
            #[cfg(target_arch = "wasm32")]
            pub mod #mod_name {
                #imports

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

impl<T, U> ToNsPath<T> for U
where
    T: ToModPathIter,
    U: ToModPathIter + ?Sized,
{
    fn to_ns_path(&self, current_mod: &T) -> TokenStream2 {
        let ns_len = current_mod.to_mod_path_iter().count();
        let mut use_path = vec![format_ident!("super").into(); ns_len];
        use_path.extend(self.to_mod_path_iter());
        quote! {
            #(#use_path)::*
        }
    }
}

fn get_recursive_fields(iface: &Interface) -> HashMap<String, TypeRef> {
    get_recursive_fields_with_type_params(iface, &Default::default())
}

fn get_recursive_fields_with_type_params(
    Interface {
        extends,
        fields,
        type_params: _, // TODO: should probably restrict resolve_generic_in_env to only fill in known generics
        ..
    }: &Interface,
    type_env: &HashMap<String, TypeRef>,
) -> HashMap<String, TypeRef> {
    let our_fields = fields
        .iter()
        .map(|(n, t)| (n.clone(), t.resolve_generic_in_env(type_env).clone()));
    let super_fields = extends
        .iter()
        .filter_map(|base| base.resolve_target_type().map(|t| (base, t)))
        .filter_map(|(base, resolved_base)| match resolved_base {
            // TODO: do we need to support non-interface super types?
            TargetEnrichedTypeInfo::Interface(iface) => Some((base, iface)),
            _ => None,
        })
        .flat_map(|(base, iface)| {
            let super_type_env = apply_type_params(base, &iface, type_env);
            get_recursive_fields_with_type_params(&iface, &super_type_env).into_iter()
        });

    our_fields.chain(super_fields).collect()
}

trait IsUninhabited {
    fn is_uninhabited(&self) -> bool;
}

impl IsUninhabited for TargetEnrichedTypeInfo {
    fn is_uninhabited(&self) -> bool {
        match self {
            TargetEnrichedTypeInfo::Ref(r) => r.is_uninhabited(),
            TargetEnrichedTypeInfo::Union(Union { types, .. }) => {
                types.iter().all(IsUninhabited::is_uninhabited)
            }
            _ => false,
        }
    }
}

impl IsUninhabited for TypeRef {
    fn is_uninhabited(&self) -> bool {
        match self.referent {
            TypeIdent::Builtin(Builtin::PrimitiveNull) => true,
            TypeIdent::Builtin(Builtin::PrimitiveUndefined) => true,
            TypeIdent::Builtin(Builtin::PrimitiveVoid) => true,
            _ => false,
        }
    }
}

fn type_to_union_case_name(typ: &TargetEnrichedTypeInfo) -> Identifier {
    let t_str = quote! { #typ }
        .to_string()
        .replace("<", "Of")
        .replace(">", "")
        .replace("&", "")
        .replace("[", "")
        .replace("]", "");
    to_camel_case_ident(format!("{}Case", t_str))
}

fn path_relative_to_cargo_toml<T: AsRef<Path>>(path: T) -> PathBuf {
    let mut best: Option<PathBuf> = None;
    let mut current_path: Option<PathBuf> = None;
    let path = path.as_ref();
    for component in path.components() {
        let p = current_path
            .map(|cp| cp.join(component))
            .unwrap_or_else(|| (component.as_ref() as &Path).to_path_buf());
        if p.is_dir() {
            if p.join("Cargo.toml").exists() {
                best = Some(p.clone());
            }
        }
        current_path = Some(p);
    }

    best.map(|p| path.components().skip(p.components().count()).collect())
        .unwrap_or_else(|| path.to_path_buf())
}

fn trim_after_dot<'a>(s: &'a str) -> &'a str {
    let idx = s.find('.');
    &s[0..idx.unwrap_or_else(|| s.len())]
}

fn get_field_count<T: FieldCountGetter>(t: &T) -> usize {
    t.get_field_count()
}

trait FieldCountGetter {
    fn get_field_count(&self) -> usize;
}

impl FieldCountGetter for TargetEnrichedType {
    fn get_field_count(&self) -> usize {
        self.info.get_field_count()
    }
}

impl FieldCountGetter for TargetEnrichedTypeInfo {
    fn get_field_count(&self) -> usize {
        // we return the field count for things that have fields and, other than that, ensure that
        // undefined will always have the lowest field count
        let min = usize::MIN;
        trait_impl_for_type_info!(
            match self,
            get_field_count | min,
            TargetEnrichedTypeInfo::Interface(i) => {
                if i.indexer.is_some() {
                    // TODO: is this what we want????
                    usize::MAX
                } else {
                    i.fields.len()
                }
            },
            TargetEnrichedTypeInfo::Enum(e) => e.members.len(),
            TargetEnrichedTypeInfo::Ref(TypeRef {
                referent: TypeIdent::Builtin(_),
                ..
            }) => min,
            TargetEnrichedTypeInfo::Array { .. } => min,
            TargetEnrichedTypeInfo::Union(u) => {
                u.types.iter().map(get_field_count).max().unwrap_or(min)
            },
            TargetEnrichedTypeInfo::Intersection(i) => {
                i.types.iter().map(get_field_count).min().unwrap_or(min)
            },
            TargetEnrichedTypeInfo::Tuple(t) => t.types.len(),
            TargetEnrichedTypeInfo::Mapped { .. } => usize::MAX,
            TargetEnrichedTypeInfo::Func(_) => min,
            TargetEnrichedTypeInfo::Constructor(_) => min,
            TargetEnrichedTypeInfo::Class(c) => {
                c.members.len()
                    + c.super_class
                        .as_ref()
                        .and_then(|s| s.resolve_target_type())
                        .as_ref()
                        .map(|t| t.get_field_count())
                        .unwrap_or(0)
            },
            TargetEnrichedTypeInfo::Var { .. } => min,
        )
    }
}

trait MemberContainer {
    fn undefined_and_standard_members(
        &self,
    ) -> (Vec<&TargetEnrichedTypeInfo>, Vec<&TargetEnrichedTypeInfo>);

    fn has_undefined_member(&self) -> bool {
        !self.undefined_and_standard_members().0.is_empty()
    }
}

impl MemberContainer for Union {
    fn undefined_and_standard_members(
        &self,
    ) -> (Vec<&TargetEnrichedTypeInfo>, Vec<&TargetEnrichedTypeInfo>) {
        self.types.iter().partition(|t| match t {
            TargetEnrichedTypeInfo::Ref(t)
                if t.referent == TypeIdent::Builtin(Builtin::PrimitiveUndefined) =>
            {
                true
            }
            _ => false,
        })
    }
}

fn is_potentially_undefined<T: UndefinedHandler>(t: &T) -> bool {
    t.is_potentially_undefined()
}

trait UndefinedHandler {
    /// Is self potentially undefined if the type were living on its own.
    /// That is, we ignore the possibility of an optional field of this type being undefined
    /// because that is a property of the field and not the type.
    fn is_potentially_undefined(&self) -> bool;
}

impl UndefinedHandler for Union {
    fn is_potentially_undefined(&self) -> bool {
        let (und, std) = self.undefined_and_standard_members();
        let has_direct_undefined_member = !und.is_empty();
        has_direct_undefined_member || std.iter().any(|t| t.is_potentially_undefined())
    }
}

impl UndefinedHandler for TypeRef {
    fn is_potentially_undefined(&self) -> bool {
        self.resolve_target_type()
            .as_ref()
            .map(is_potentially_undefined)
            .unwrap_or(false)
    }
}

impl UndefinedHandler for TargetEnrichedTypeInfo {
    fn is_potentially_undefined(&self) -> bool {
        trait_impl_for_type_info!(
            match self,
            is_potentially_undefined | false,
            TargetEnrichedTypeInfo::Interface(_) => false,
            TargetEnrichedTypeInfo::Enum(_) => false,
            TargetEnrichedTypeInfo::Ref(TypeRef {
                referent: TypeIdent::Builtin(
                    Builtin::PrimitiveUndefined
                    | Builtin::PrimitiveAny
                    | Builtin::PrimitiveObject
                    | Builtin::PrimitiveVoid,
                ),
                ..
            }) => true,
            TargetEnrichedTypeInfo::Ref(TypeRef {
                referent: TypeIdent::Builtin(_),
                ..
            }) => false,
            TargetEnrichedTypeInfo::Array { .. } => false,
            TargetEnrichedTypeInfo::Optional { .. } => true,
            TargetEnrichedTypeInfo::Union(u) => u.is_potentially_undefined(),
            TargetEnrichedTypeInfo::Intersection(i) => i.types.iter().any(is_potentially_undefined),
            TargetEnrichedTypeInfo::Tuple(_) => false,
            TargetEnrichedTypeInfo::Mapped { .. } => false,
            TargetEnrichedTypeInfo::Func(_) => false,
            TargetEnrichedTypeInfo::Constructor(_) => false,
            TargetEnrichedTypeInfo::Class(_) => false,
            TargetEnrichedTypeInfo::Var { .. } => false,
        )
    }
}

trait JsModulePath {
    fn js_module_path(&self) -> String;
}

impl JsModulePath for Context {
    fn js_module_path(&self) -> String {
        let path = &self.path;
        let path = path_relative_to_cargo_toml(path.with_file_name(
            trim_after_dot(&*path.file_name().unwrap().to_string_lossy()).to_string() + ".js",
        ));
        path.to_string_lossy().to_string()
    }
}

impl ToTokens for TargetEnrichedType {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let (js_name, name) = self.name.to_name();
        let vis = if self.is_exported {
            let vis = format_ident!("pub");
            quote! { #vis }
        } else {
            quote! {}
        };

        let our_toks = match &self.info {
            TargetEnrichedTypeInfo::Interface(iface) => {
                let Interface {
                    indexer,
                    type_params,
                    ..
                } = iface;
                let extended_fields = get_recursive_fields(iface);

                let full_type_params = render_type_params(type_params);
                let mut field_toks = extended_fields
                    .iter()
                    .map(|(js_field_name, typ)| {
                        let field = FieldDefinition {
                            self_name: &name,
                            js_field_name,
                            typ,
                            type_params: &type_params.iter().map(|(k, v)| (k.clone(), v)).collect(),
                        };
                        quote! { #field }
                    })
                    .collect::<Vec<TokenStream2>>();

                let serializers: Vec<_> = extended_fields
                    .iter()
                    .filter_map(|(js_field_name, typ)| {
                        typ.resolve_target_type()
                            .map(|t| (to_snake_case_ident(js_field_name), t))
                    })
                    .filter_map(|(field_name, typ)| render_serialize_fn(&field_name, &typ))
                    .collect();
                let deserializers: Vec<_> = extended_fields
                    .iter()
                    .filter_map(|(js_field_name, typ)| {
                        typ.resolve_target_type()
                            .map(|t| (to_snake_case_ident(js_field_name), t))
                    })
                    .filter_map(|(field_name, typ)| render_deserialize_fn(&field_name, &typ))
                    .collect();
                let serializer_impl = if serializers.is_empty() && deserializers.is_empty() {
                    quote! {}
                } else {
                    quote! {
                        impl #name {
                            #(#serializers)*
                            #(#deserializers)*
                        }
                    }
                };

                if let Some(Indexer {
                    readonly: _,
                    value_type,
                    ..
                }) = &indexer
                {
                    let extra_fields_name = to_unique_ident("extra_fields".to_string(), &|x| {
                        extended_fields.contains_key(x)
                    });

                    field_toks.push(quote! {
                        #[serde(flatten)]
                        pub #extra_fields_name: std::collections::HashMap<String, #value_type>
                    });
                }
                let trait_defn =
                    render_trait_defn(&name, &js_name, type_params, iface, &iface.context);

                quote! {
                    #[derive(Clone, serde::Serialize, serde::Deserialize)]
                    pub struct #name #full_type_params {
                        #(#field_toks),*
                    }

                    #trait_defn

                    #serializer_impl
                }
            }
            TargetEnrichedTypeInfo::Enum(Enum { members, .. }) => {
                quote! {
                    #[wasm_bindgen]
                    #[derive(Clone, serde::Serialize, serde::Deserialize)]
                    #[serde(untagged)]
                    pub enum #name {
                        #(#members),*
                    }
                }
            }
            TargetEnrichedTypeInfo::Alias(Alias {
                target,
                type_params,
                ..
            }) => {
                let tps = render_type_params(type_params);

                quote! {
                    #[allow(dead_code)]
                    #vis type #name #tps = #target;
                }
            }
            //TargetEnrichedTypeInfo::Ref(_) => panic!("ref isn't a top-level type"),
            //TargetEnrichedTypeInfo::Array { .. } => panic!("Array isn't a top-level type"),
            //TargetEnrichedTypeInfo::Optional { .. } => panic!("Optional isn't a top-level type"),
            TargetEnrichedTypeInfo::Union(u) => {
                let (undefined_members, mut not_undefined_members) =
                    u.undefined_and_standard_members();

                // members must be sorted in order of decreasing number of fields to ensure that we
                // deserialize unions into the "larger" variant in case of overlaps
                not_undefined_members.sort_by_key(|m| get_field_count(*m));
                not_undefined_members.reverse();
                let member_cases = not_undefined_members
                    .iter()
                    .map(|t| {
                        let case = type_to_union_case_name(t);

                        if t.is_uninhabited() {
                            quote! {
                                #case
                            }
                        } else {
                            quote! {
                                #case(#t)
                            }
                        }
                    })
                    .chain(undefined_members.iter().map(|t| {
                        let case = type_to_union_case_name(t);

                        quote! {
                            #[serde(serialize_with="ts_bindgen_rt::serialize_undefined", deserialize_with="ts_bindgen_rt::deserialize_undefined")]
                            #case
                        }
                    }));

                quote! {
                    #[derive(Clone, serde::Serialize, serde::Deserialize)]
                    #[serde(untagged)]
                    pub enum #name {
                        #(#member_cases),*
                    }
                }
            }
            TargetEnrichedTypeInfo::Tuple(Tuple { types, .. }) => {
                quote! {
                    #[derive(Clone, serde::Serialize, serde::Deserialize)]
                    pub struct #name(#(pub #types),*);
                }
            }
            TargetEnrichedTypeInfo::Func(func) => {
                let path = func.context.js_module_path();
                let attrs = {
                    let mut attrs = vec![quote! { js_name = #js_name, catch }];
                    if func.is_variadic() {
                        attrs.push(quote! { variadic });
                    }
                    attrs
                };
                let internal_func = InternalFunc { js_name, func };
                let wrapper_func = WrapperFunc { js_name, func };

                quote! {
                    #[wasm_bindgen(module=#path)]
                    extern "C" {
                        #[wasm_bindgen(#(#attrs),*)]
                        #internal_func
                    }

                    #wrapper_func
                }
            }
            TargetEnrichedTypeInfo::Class(class) => {
                let Class {
                    super_class,
                    members,
                    context,
                    type_params,
                    implements: _,
                } = class;
                let path = context.js_module_path();
                let mut attrs = vec![quote! { js_name = #js_name }];
                if let Some(TypeRef { referent, .. }) = super_class.as_ref() {
                    let (_, super_name) = referent.to_name();

                    attrs.push(quote! {
                        extends = #super_name
                    });
                }

                let member_defs = members.iter().map(|(member_js_name, member)| {
                    let member_js_ident = format_ident!("{}", member_js_name);
                    match member {
                        Member::Constructor(ctor) => {
                            let ctor = Constructor::new(
                                Cow::Borrowed(ctor),
                                TypeIdent::LocalName(js_name.to_string()),
                            );
                            let param_toks = ctor
                                .params()
                                .map(|p| p.as_exposed_to_rust_named_param_list());

                            quote! {
                                #[wasm_bindgen(constructor)]
                                pub fn new(#(#param_toks),*) -> #name;
                            }
                        }
                        Member::Method(func) => {
                            let fn_name = InternalFunc::to_internal_rust_name(member_js_name);

                            let f = func.exposed_to_js_fn_decl(fn_name);

                            let mut attrs = vec![
                                quote! {js_name = #member_js_ident},
                                quote! {method},
                                quote! {js_class = #js_name},
                                quote! {catch},
                            ];
                            if func.is_variadic() {
                                attrs.push(quote! { variadic });
                            }

                            quote! {
                                #[wasm_bindgen(#(#attrs),*)]
                                #f;
                            }
                        }
                        Member::Property(typ) => {
                            let member_name = to_snake_case_ident(member_js_name);
                            let setter_name = format_ident!("set_{}", member_name.to_string());
                            // TODO: don't add structural if the property is actually a
                            // javascript getter/setter
                            quote! {
                                #[wasm_bindgen(method, structural, getter = #member_js_ident)]
                                fn #member_name(this: &#name) -> #typ;

                                #[wasm_bindgen(method, structural, setter = #member_js_ident)]
                                fn #setter_name(this: &#name, value: #typ);
                            }
                        }
                    }
                });

                let public_methods = members
                    .iter()
                    .filter_map(|(js_name, member)| match member {
                        Member::Method(m) => Some((js_name, m)),
                        _ => None,
                    })
                    .map(|(js_name, method)| {
                        let fn_name = to_snake_case_ident(js_name);
                        let internal_fn_name = InternalFunc::to_internal_rust_name(js_name);
                        method.exposed_to_rust_wrapper_fn(&fn_name, &internal_fn_name)
                    });

                let trait_defn =
                    render_trait_defn(&name, &js_name, &type_params, class, &class.context);

                quote! {
                    #[wasm_bindgen(module = #path)]
                    extern "C" {
                        #[wasm_bindgen(#(#attrs),*)]
                        #vis type #name;

                        #(#member_defs)*
                    }

                    impl #name {
                        #(#public_methods)*
                    }

                    #trait_defn

                    impl Clone for #name {
                        fn clone(&self) -> Self {
                            Self { obj: self.obj.clone() }
                        }
                    }

                    impl serde::ser::Serialize for #name {
                        fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
                        where
                            S: serde::ser::Serializer,
                        {
                            ts_bindgen_rt::serialize_as_jsvalue(serializer, self)
                        }
                    }

                    impl<'de> serde::de::Deserialize<'de> for #name {
                        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
                        where
                            D: serde::de::Deserializer<'de>,
                        {
                            ts_bindgen_rt::deserialize_as_jsvalue(deserializer)
                        }
                    }
                }
            }
            TargetEnrichedTypeInfo::Intersection(isect) => {
                if let Some(first_type) = isect.types.first().and_then(|t| t.resolve_target_type())
                {
                    if let TargetEnrichedTypeInfo::Interface(_) = first_type {
                        let fields = isect
                            .types
                            .iter()
                            .filter_map(|t| t.resolve_target_type())
                            .filter_map(|t| {
                                if let TargetEnrichedTypeInfo::Interface(iface) = t {
                                    Some(iface)
                                } else {
                                    None
                                }
                            })
                            .flat_map(|iface| get_recursive_fields(&iface))
                            .collect();

                        let indexer = isect
                            .types
                            .iter()
                            .filter_map(|t| t.resolve_target_type())
                            .filter_map(|t| {
                                if let TargetEnrichedTypeInfo::Interface(iface) = t {
                                    Some(iface)
                                } else {
                                    None
                                }
                            })
                            .filter_map(|iface| iface.indexer)
                            .next();

                        let typ = TargetEnrichedType {
                            name: self.name.clone(),
                            is_exported: self.is_exported,
                            info: TargetEnrichedTypeInfo::Interface(Interface {
                                indexer,
                                fields,
                                extends: Default::default(),
                                context: isect.context.clone(),
                                type_params: Default::default(), // TODO: copy over type params from isect
                            }),
                            context: isect.context.clone(),
                        };

                        quote! {
                            #typ
                        }
                    } else {
                        // TODO: this is weird, do we ever run into trouble with this?
                        let mut typ = self.clone();
                        typ.info = first_type.clone();
                        quote! {
                            #typ
                        }
                    }
                } else {
                    panic!("Intersections must not be empty");
                }
            }
            /*TypeInfo::Mapped {
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
            },
            TypeInfo::Constructor {
                params: Vec<Param>,
                return_type: Box<TypeInfo>,
            },
            TypeInfo::Var {
                type_info: Box<TypeInfo>,
            },*/
            TargetEnrichedTypeInfo::NamespaceImport(NamespaceImport::All { src, .. }) => {
                let ns = src.as_path().to_ns_path(&self.name);
                let name = to_snake_case_ident(js_name);

                quote! {
                    #vis use #ns as #name;
                }
            }
            TargetEnrichedTypeInfo::NamespaceImport(NamespaceImport::Default { src, .. }) => {
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
            }
            TargetEnrichedTypeInfo::NamespaceImport(NamespaceImport::Named {
                src,
                name: item_name,
                ..
            }) => {
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
            }
            _ => {
                quote! {}
            }
        };

        toks.append_all(our_toks);
    }
}

impl ToTokens for TargetEnrichedTypeInfo {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let our_toks = match &self {
            TargetEnrichedTypeInfo::Interface(_) => {
                panic!("interface in type info");
            }
            TargetEnrichedTypeInfo::Enum(_) => {
                panic!("enum in type info");
            }
            TargetEnrichedTypeInfo::Ref(TypeRef {
                referent,
                type_params: _,
                ..
            }) => {
                let (_, local_name) = referent.to_name();
                quote! {
                    #local_name
                }
            }
            TargetEnrichedTypeInfo::Alias(Alias { target, .. }) => {
                // TODO: we should get the name of the alias, not the ponited-to name
                quote! { #target }
            }
            TargetEnrichedTypeInfo::Array { item_type, .. } => {
                quote! {
                    Vec<#item_type>
                }
            }
            TargetEnrichedTypeInfo::Optional { item_type, .. } => {
                quote! {
                    Option<#item_type>
                }
            }
            TargetEnrichedTypeInfo::Union(Union { types: _, .. }) => {
                quote! {}
            }
            TargetEnrichedTypeInfo::Intersection(Intersection { types: _, .. }) => {
                // TODO
                quote! {}
            }
            TargetEnrichedTypeInfo::Mapped { value_type, .. } => {
                quote! {
                    std::collections::HashMap<String, #value_type>
                }
            }
            TargetEnrichedTypeInfo::Func(Func {
                params,
                type_params: _,
                return_type,
                ..
            }) => {
                let param_toks: Vec<TokenStream2> = params
                    .iter()
                    .map(|p| {
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
                    })
                    .collect();

                quote! {
                    fn(#(#param_toks),*) -> #return_type
                }
            }
            /*
            TargetEnrichedTypeInfo::Constructor {
                params: Vec<Param>,
                return_type: Box<TypeInfo>,
            },
            TargetEnrichedTypeInfo::Class(Class {
                members: HashMap<String, Member>,
            }),
            TargetEnrichedTypeInfo::Var {
                type_info: Box<TypeInfo>,
            },
            TargetEnrichedTypeInfo::GenericType {
                name: String,
                constraint: Box<TypeInfo>,
            },*/
            TargetEnrichedTypeInfo::NamespaceImport(_) => panic!("namespace import in type info"),
            _ => {
                quote! {}
            }
        };

        toks.append_all(our_toks);
    }
}

impl ToTokens for TypeRef {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let our_toks = {
            let (_, name) = self.referent.to_name();
            if matches!(&self.referent, TypeIdent::Builtin(Builtin::Fn)) {
                let params = self
                    .params()
                    .map(|p| p.as_exposed_to_rust_unnamed_param_list());
                let ret = fn_types::exposed_to_rust_return_type(&self.return_type());
                quote! {
                    dyn #name(#(#params),*) -> Result<#ret, JsValue>
                }
            } else if matches!(&self.referent, TypeIdent::Builtin(Builtin::PrimitiveVoid)) {
                quote! { () }
            } else if self.type_params.is_empty() {
                quote! { #name }
            } else {
                let type_params = self.type_params.iter().map(|p| quote! { #p });
                quote! { #name<#(#type_params),*> }
            }
        };

        toks.append_all(our_toks);
    }
}

struct FieldDefinition<'a> {
    self_name: &'a Identifier,
    js_field_name: &'a str,
    typ: &'a TypeRef,
    type_params: &'a HashMap<String, &'a TypeParamConfig>,
}

impl<'a> ToTokens for FieldDefinition<'a> {
    fn to_tokens(&self, toks: &mut TokenStream2) {
        let js_field_name = self.js_field_name;
        let field_name = to_snake_case_ident(js_field_name);
        let typ = self.typ;
        let (_, type_name) = typ.referent.to_name();
        let type_name = type_name.to_string();
        let type_param = self.type_params.get(&type_name);
        let mut serde_attrs = vec![quote! { rename = #js_field_name }];
        if type_param.is_some() {
            let bound = format!(
                "{}: Clone + serde::Serialize + serde::Deserialize<'de>",
                &type_name
            );
            let attr = quote! {
                bound(deserialize = #bound)
            };
            serde_attrs.push(attr);
        }
        let rendered_type = OwnedTypeRef(Cow::Borrowed(typ));

        if typ.serialization_type() == SerializationType::Fn {
            let serialize_fn = field_name.prefix_name("__tsb__serialize_");
            let deserialize_fn = field_name.prefix_name("__tsb__deserialize_");
            let serialize_fn = format!("{}::{}", self.self_name, serialize_fn);
            let deserialize_fn = format!("{}::{}", self.self_name, deserialize_fn);
            serde_attrs.push(quote! {
                serialize_with = #serialize_fn
            });
            serde_attrs.push(quote! {
                deserialize_with = #deserialize_fn
            });
        };

        let our_toks = quote! {
            #[serde(#(#serde_attrs),*)]
            pub #field_name: #rendered_type
        };

        toks.append_all(our_toks);
    }
}

fn render_deserialize_fn(
    field_name: &Identifier,
    type_info: &TargetEnrichedTypeInfo,
) -> Option<TokenStream2> {
    if let TargetEnrichedTypeInfo::Ref(
        tr
        @ TypeRef {
            referent: TypeIdent::Builtin(Builtin::Fn),
            ..
        },
    ) = type_info
    {
        let deserialize_fn_name = field_name.prefix_name("__tsb__deserialize_");
        let return_type = tr.return_type();
        let return_value = quote! { ret };
        let ret = render_raw_return_to_js(&return_type, &return_value);
        let args = quote! { args };
        let params = tr.params().map(|p| p.as_exposed_to_rust_named_param_list());
        let rendered_type = OwnedTypeRef(Cow::Borrowed(tr));
        // TODO: need to render wrappers for fn params, used in rust_to_jsvalue_conversion
        let conversions = tr.args().map(|p| {
            let name = p.rust_name();
            let conv = p.rust_to_jsvalue_conversion();
            quote! {
                let #name = #conv;
            }
        });
        let pushes = tr.params().map(|p| {
            let name = p.rust_name();
            quote! {
                #args.push(&#name);
            }
        });
        // TODO: do we need to handle member functions here (first arg to apply may be
        // non-null)
        Some(quote! {
            #[allow(non_snake_case)]
            fn #deserialize_fn_name<'de, D>(deserializer: D) -> std::result::Result<#rendered_type, D::Error>
            where
                D: serde::de::Deserializer<'de>,
            {
                let jsv: JsValue = ts_bindgen_rt::deserialize_as_jsvalue(deserializer)?;
                let #field_name: Option<&js_sys::Function> = wasm_bindgen::JsCast::dyn_ref(&jsv);
                Ok(#field_name.map(|f| {
                    let f = f.clone();
                    std::rc::Rc::new(move |#(#params),*| {
                        #(#conversions);*
                        let args = js_sys::Array::new();
                        #(#pushes);*
                        let #return_value = f.apply(&JsValue::null(), &args)?;
                        Ok(#ret)
                    }) as #rendered_type
                })
                .ok_or_else(|| ts_bindgen_rt::jsvalue_serde::Error::InvalidType("expected function".to_string()))
                .map_err(serde::de::Error::custom)?)
            }
        })
    } else {
        None
    }
}

fn render_serialize_fn(
    field_name: &Identifier,
    type_info: &TargetEnrichedTypeInfo,
) -> Option<TokenStream2> {
    if let TargetEnrichedTypeInfo::Ref(
        tr
        @ TypeRef {
            referent: TypeIdent::Builtin(Builtin::Fn),
            ..
        },
    ) = type_info
    {
        let serialize_fn_name = field_name.prefix_name("__tsb__serialize_");
        let invocation = tr.invoke_with_name(field_name);
        let closure = tr.exposed_to_js_wrapped_closure(invocation);
        let rendered_type = OwnedTypeRef(Cow::Borrowed(tr));
        Some(quote! {
            #[allow(non_snake_case)]
            fn #serialize_fn_name<S>(#field_name: &#rendered_type, serializer: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: serde::ser::Serializer,
            {
                let #field_name = #field_name.clone();
                let #field_name = #closure;
                let jsv = ts_bindgen_rt::serialize_as_jsvalue(serializer, &#field_name.into_js_value());
                //#field_name.forget(); // TODO: how do we properly handle memory management?
                jsv
            }
        })
    } else {
        None
    }
}

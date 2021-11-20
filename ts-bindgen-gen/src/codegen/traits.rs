use crate::codegen::funcs::{AccessType, Constructor, FnPrototypeExt, PropertyAccessor};
use crate::codegen::generics::{
    render_type_params, render_type_params_with_constraints, NameWithGenericEnv, WithTypeEnv,
};
use crate::codegen::generics::{ResolveGeneric, TypeEnvImplying};
use crate::codegen::named::Named;
use crate::codegen::resolve_target_type::ResolveTargetType;
use crate::identifier::{make_identifier, to_snake_case_ident, Identifier};
use crate::ir::{
    Builtin, Class, Context, Func, Interface, Member, Param, TargetEnrichedTypeInfo, TypeIdent,
    TypeParamConfig, TypeRef,
};
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use std::borrow::Cow;
use std::collections::HashMap;
use std::iter;

#[derive(Debug, Clone)]
pub enum TraitMember<'a> {
    Constructor {
        name: Identifier,
        ctor: Constructor<'a>,
    },
    Method {
        name: Identifier,
        method: Func,
    },
    Getter {
        name: Identifier,
        prop: PropertyAccessor,
    },
    Setter {
        name: Identifier,
        prop: PropertyAccessor,
    },
}

impl<'a> TraitMember<'a> {
    fn name(&'a self) -> &'a Identifier {
        match self {
            TraitMember::Constructor { name, .. } => name,
            TraitMember::Method { name, .. } => name,
            TraitMember::Getter { name, .. } => name,
            TraitMember::Setter { name, .. } => name,
        }
    }
}

pub fn render_trait_defn<T>(
    name: &Identifier,
    js_name: &str,
    type_params: &[(String, TypeParamConfig)],
    item: &T,
    ctx: &Context,
) -> TokenStream2
where
    T: Traitable,
{
    let tps = render_type_params(type_params);
    let full_name = quote! {
        #name #tps
    };
    let tps_with_constraints =
        render_type_params_with_constraints(type_params, vec![quote! { std::clone::Clone }]);
    let class_name = || TypeIdent::LocalName(js_name.to_string());
    let item_ref = TypeRef {
        referent: class_name(),
        type_params: type_params
            .iter()
            .map(|(n, _)| TypeRef {
                referent: TypeIdent::LocalName(n.clone()),
                type_params: Default::default(),
                context: ctx.clone(),
            })
            .collect(),
        context: ctx.clone(),
    };
    let member_to_trait_member = |type_env: &HashMap<String, TypeRef>, (n, m): (String, Member)| {
        let name = to_snake_case_ident(&n);
        match m {
            Member::Constructor(ctor) => {
                let ctor = Constructor {
                    class: Cow::Borrowed(&item_ref),
                    ctor: Cow::Owned(ctor),
                };
                vec![TraitMember::Constructor {
                    name: make_identifier!(new),
                    ctor,
                }]
            }
            Member::Method(f) => vec![TraitMember::Method { name, method: f }],
            Member::Property(t) => {
                let getter = PropertyAccessor {
                    property_name: name.clone(),
                    typ: t.resolve_generic_in_env(type_env).clone(),
                    class_name: class_name(),
                    access_type: AccessType::Getter,
                };
                let setter = PropertyAccessor {
                    property_name: name.clone(),
                    typ: t.resolve_generic_in_env(type_env).clone(),
                    class_name: class_name(),
                    access_type: AccessType::Setter,
                };
                vec![
                    TraitMember::Getter { name, prop: getter },
                    TraitMember::Setter {
                        name: to_snake_case_ident(format!("set_{}", n)),
                        prop: setter,
                    },
                ]
            }
        }
    };
    let (super_decl, super_impls) = if item.has_super_traits() {
        // TODO: would be nice to mark Identifiers with the type of identifier they are
        // e.g. mark anything coming out of a TraitName::trait_name as a trait
        // identifier
        let supers = item.super_traits().map(|s| s.trait_name());
        let super_decl = quote! {
            : #(#supers)+*
        };
        let make_impl = |i: Super| {
            let tr = &i.item;
            let trait_name = tr.trait_name();
            let supers: Vec<_> = tr
                .super_traits()
                .map(|s| {
                    s.trait_name()
                        .name_with_generic_env(tr.type_params.as_slice())
                })
                .collect();
            let preds = if supers.is_empty() {
                quote! {}
            } else {
                quote! {
                    where #full_name: #(#supers)+*
                }
            };
            let method_impls = tr
                .methods()
                .flat_map(|(n, m)| {
                    member_to_trait_member(&tr.type_env(), (n, m.clone())).into_iter()
                })
                .map(|trait_member| {
                    let proto = trait_member.exposed_to_rust_fn_decl(trait_member.name());
                    let imp = item.wrap_invocation(&i.implementor, &trait_member);
                    quote! {
                        #proto {
                            #imp
                        }
                    }
                });
            quote! {
                impl #tps_with_constraints #trait_name for #full_name #preds {
                    #(#method_impls)*
                }
            }
        };
        let super_impls = item
            .recursive_super_traits(item_ref.clone(), &vec![])
            .chain(Box::new(iter::once(Super {
                item: item_ref.clone(),
                implementor: item_ref.clone(),
            })))
            .map(&make_impl);
        let super_impls = quote! {
            #(#super_impls)*
        };

        (super_decl, super_impls)
    } else {
        (quote! {}, quote! {})
    };

    let method_decls = item
        .methods()
        .flat_map(|nm| member_to_trait_member(&Default::default(), nm))
        .map(|f| f.exposed_to_rust_fn_decl(f.name()))
        .map(|t| {
            quote! {
                #t;
            }
        });

    let trait_name = name.trait_name();

    quote! {
        trait #trait_name #tps #super_decl {
            #(#method_decls)*
        }

        #super_impls
    }
}

/// Represents a superclass or implemented interface.
pub struct Super {
    /// `item` is the superclass or implemented interface.
    item: TypeRef,
    /// `implementor` is set to the type that contains the actual implementation.
    /// For example, a typescript interface, `I`, embeds all members from extended
    /// interfaces directly within it so implementor will always refer to
    /// `I`.
    /// However, a typescript class, `C`, which extends a class `Base`, will not
    /// contain `Base`'s members so when `Super::item` is set to `Base` or an
    /// interface implemented by `Base`, `Super::implementor` will be set
    /// to `Base`.
    implementor: TypeRef,
}

type BoxedSuperIter<'a> = Box<dyn Iterator<Item = Super> + 'a>;
type BoxedTypeRefIter<'a> = Box<dyn Iterator<Item = TypeRef> + 'a>;
type BoxedMemberIter<'a> = Box<dyn Iterator<Item = (String, Member)> + 'a>;

pub trait Traitable {
    fn has_super_traits(&self) -> bool;

    fn super_traits<'a>(&'a self) -> BoxedTypeRefIter<'a>;

    fn methods<'a>(&'a self) -> BoxedMemberIter<'a>;

    fn contains_implementation(&self) -> bool;

    fn wrap_invocation(
        &self,
        member_defn_source: &TypeRef,
        trait_member: &TraitMember,
    ) -> TokenStream2;

    fn recursive_super_traits<'a>(
        &'a self,
        implementor: TypeRef,
        type_env: &[TypeRef],
    ) -> BoxedSuperIter<'a> {
        Box::new(self.super_traits().fold(
            Box::new(iter::empty()) as BoxedSuperIter<'a>,
            |cur, s| {
                let s = s.with_type_env(type_env);
                let implementor = if s.contains_implementation() {
                    s.clone()
                } else {
                    implementor.clone()
                };
                let supers = s
                    .resolve_target_type()
                    .map(|t| {
                        Box::new(
                            t.recursive_super_traits(implementor.clone(), &s.type_params)
                                .collect::<Vec<_>>()
                                .into_iter(),
                        ) as BoxedSuperIter<'a>
                    })
                    .unwrap_or_else(|| Box::new(iter::empty()) as BoxedSuperIter<'a>);
                let s_iter = Box::new(iter::once(Super {
                    item: s.clone(),
                    implementor,
                })) as BoxedSuperIter<'a>;

                Box::new(cur.chain(s_iter).chain(supers)) as BoxedSuperIter<'a>
            },
        )) as BoxedSuperIter<'a>
    }
}

impl Traitable for Interface {
    fn has_super_traits(&self) -> bool {
        !self.extends.is_empty()
    }

    fn super_traits<'a>(&'a self) -> BoxedTypeRefIter<'a> {
        Box::new(self.extends.iter().cloned()) as BoxedTypeRefIter<'a>
    }

    fn methods<'a>(&'a self) -> BoxedMemberIter<'a> {
        Box::new(
            self.fields
                .iter()
                .map(|(n, t)| (n.clone(), Member::Property(t.clone()))),
        )
    }

    fn contains_implementation(&self) -> bool {
        // interfaces in an inheritance tree do not contain implementation,
        // their implementation is denormalized onto the root item
        false
    }

    fn wrap_invocation(
        &self,
        member_defn_source: &TypeRef,
        trait_member: &TraitMember,
    ) -> TokenStream2 {
        let class_name = &member_defn_source.referent;
        let name = trait_member.name();
        let cn = member_defn_source.to_name().1;
        let fq_name = &name.in_namespace(&cn);
        let slf = quote! { self };
        match trait_member {
            TraitMember::Constructor { ctor, .. } => {
                Constructor::new(Cow::Borrowed(&ctor.ctor), class_name.clone())
                    .fully_qualified_invoke_with_name(fq_name, Some(slf))
            }
            TraitMember::Method { method, .. } => {
                method.fully_qualified_invoke_with_name(fq_name, Some(slf))
            }
            TraitMember::Getter { prop, .. } => {
                let property_name = &prop.property_name;
                quote! {
                    std::result::Result::Ok(self.#property_name.clone())
                }
            }
            TraitMember::Setter { prop, .. } => {
                let property_name = &prop.property_name;
                quote! {
                    self.#property_name = value;
                    std::result::Result::Ok(())
                }
            }
        }
    }
}

impl Traitable for Class {
    fn has_super_traits(&self) -> bool {
        self.super_class.is_some() || !self.implements.is_empty()
    }

    fn super_traits<'a>(&'a self) -> BoxedTypeRefIter<'a> {
        let super_class = self
            .super_class
            .as_ref()
            .map(|s| Box::new(iter::once(s).cloned()) as BoxedTypeRefIter<'a>)
            .unwrap_or_else(|| Box::new(iter::empty()) as BoxedTypeRefIter<'a>);
        let implements = self.implements.iter().cloned();

        Box::new(super_class.chain(implements))
    }

    fn methods<'a>(&'a self) -> BoxedMemberIter<'a> {
        Box::new(self.members.iter().map(|(n, m)| (n.clone(), m.clone())))
    }

    fn contains_implementation(&self) -> bool {
        true
    }

    fn wrap_invocation(
        &self,
        member_defn_source: &TypeRef,
        trait_member: &TraitMember,
    ) -> TokenStream2 {
        let class_name = &member_defn_source.referent;
        let cn = member_defn_source.to_name().1;
        let name = trait_member.name();
        let name = &name.in_namespace(&cn);
        let slf = quote! { self };
        let target = quote! { target };
        match trait_member {
            TraitMember::Constructor { ctor, .. } => {
                Constructor::new(Cow::Borrowed(&ctor.ctor), class_name.clone())
                    .fully_qualified_invoke_with_name(name, None as Option<TokenStream2>)
            }
            TraitMember::Method { method, .. } => {
                let inv = method.fully_qualified_invoke_with_name(name, Some(&target));
                quote! {
                    let #target: &#member_defn_source = #slf.as_ref();
                    #inv
                }
            }
            TraitMember::Getter { prop, .. } => {
                let f = Func {
                    type_params: Default::default(),
                    params: Default::default(),
                    return_type: Box::new(prop.typ.clone()),
                    class_name: Some(class_name.clone()),
                    context: prop.typ.context.clone(),
                };
                let inv = f.fully_qualified_invoke_with_name(name, Some(&target));
                quote! {
                    let #target: &#member_defn_source = #slf.as_ref();
                    std::result::Result::Ok(#inv)
                }
            }
            TraitMember::Setter { prop, .. } => {
                let f = Func {
                    type_params: Default::default(),
                    params: vec![Param {
                        name: "value".to_string(),
                        type_info: prop.typ.clone(),
                        is_variadic: false,
                        context: prop.typ.context.clone(),
                    }],
                    return_type: Box::new(TypeRef {
                        referent: TypeIdent::Builtin(Builtin::PrimitiveVoid),
                        type_params: Default::default(),
                        context: prop.typ.context.clone(),
                    }),
                    class_name: Some(class_name.clone()),
                    context: prop.typ.context.clone(),
                };
                let inv = f.fully_qualified_invoke_with_name(name, Some(&target));
                quote! {
                    let #target: &#member_defn_source = #slf.as_ref();
                    #inv;
                    Ok(())
                }
            }
        }
    }
}

macro_rules! delegate_traitable_for_type_info {
    ($self:ident, $x:ident, $invocation:expr, $default:expr $(,)?) => {
        match $self {
            TargetEnrichedTypeInfo::Class($x) => $invocation,
            TargetEnrichedTypeInfo::Interface($x) => $invocation,
            _ => $default,
        }
    };
}

impl Traitable for TargetEnrichedTypeInfo {
    fn has_super_traits(&self) -> bool {
        delegate_traitable_for_type_info!(self, x, x.has_super_traits(), false)
    }

    fn super_traits<'a>(&'a self) -> BoxedTypeRefIter<'a> {
        delegate_traitable_for_type_info!(
            self,
            x,
            x.super_traits(),
            Box::new(iter::empty()) as BoxedTypeRefIter<'a>,
        )
    }

    fn methods<'a>(&'a self) -> BoxedMemberIter<'a> {
        delegate_traitable_for_type_info!(
            self,
            x,
            x.methods(),
            Box::new(iter::empty()) as BoxedMemberIter<'a>,
        )
    }

    fn contains_implementation(&self) -> bool {
        delegate_traitable_for_type_info!(self, x, x.contains_implementation(), false,)
    }

    fn wrap_invocation(
        &self,
        member_defn_source: &TypeRef,
        trait_member: &TraitMember,
    ) -> TokenStream2 {
        let empty = quote! {};
        delegate_traitable_for_type_info!(
            self,
            x,
            x.wrap_invocation(member_defn_source, trait_member),
            empty,
        )
    }
}

impl Traitable for TypeRef {
    fn has_super_traits(&self) -> bool {
        self.resolve_target_type()
            .map(|t| t.has_super_traits())
            .unwrap_or(false)
    }

    fn super_traits<'a>(&'a self) -> BoxedTypeRefIter<'a> {
        self.resolve_target_type()
            .map(|t| {
                Box::new(t.super_traits().collect::<Vec<_>>().into_iter()) as BoxedTypeRefIter<'a>
            })
            .unwrap_or_else(|| Box::new(iter::empty()) as BoxedTypeRefIter<'a>)
    }

    fn methods<'a>(&'a self) -> BoxedMemberIter<'a> {
        self.resolve_target_type()
            .map(|t| Box::new(t.methods().collect::<Vec<_>>().into_iter()) as BoxedMemberIter<'a>)
            .unwrap_or_else(|| Box::new(iter::empty()) as BoxedMemberIter<'a>)
    }

    fn contains_implementation(&self) -> bool {
        self.resolve_target_type()
            .map(|t| t.contains_implementation())
            .unwrap_or(false)
    }

    fn wrap_invocation(
        &self,
        member_defn_source: &TypeRef,
        trait_member: &TraitMember,
    ) -> TokenStream2 {
        self.resolve_target_type()
            .map(|t| t.wrap_invocation(member_defn_source, trait_member))
            .unwrap_or_else(|| quote! {})
    }
}

trait TraitName {
    fn trait_name(&self) -> Identifier;
}

impl TraitName for Identifier {
    fn trait_name(&self) -> Identifier {
        self.suffix_name("Trait")
    }
}

impl TraitName for TypeRef {
    fn trait_name(&self) -> Identifier {
        self.to_name().1.trait_name()
    }
}

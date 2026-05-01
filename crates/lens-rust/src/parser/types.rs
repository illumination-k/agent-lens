//! Type walk: collect type-path tokens from `syn::Type` trees.
//!
//! Used by signature analysis to record the type names that flow through a
//! function's parameters and return type, looking through references,
//! generics, trait bounds, and `Fn(..) -> ..` parenthesized arguments.

use super::path_to_string;

pub(super) fn collect_type_paths(ty: &syn::Type, out: &mut Vec<String>) {
    match ty {
        syn::Type::Array(array) => collect_type_paths(&array.elem, out),
        syn::Type::BareFn(bare_fn) => {
            for input in &bare_fn.inputs {
                collect_type_paths(&input.ty, out);
            }
            if let syn::ReturnType::Type(_, ty) = &bare_fn.output {
                collect_type_paths(ty, out);
            }
        }
        syn::Type::Group(group) => collect_type_paths(&group.elem, out),
        syn::Type::ImplTrait(impl_trait) => {
            for bound in &impl_trait.bounds {
                collect_bound_paths(bound, out);
            }
        }
        syn::Type::Macro(mac) => out.push(path_to_string(&mac.mac.path)),
        syn::Type::Paren(paren) => collect_type_paths(&paren.elem, out),
        syn::Type::Path(type_path) => {
            out.push(path_to_string(&type_path.path));
            for segment in &type_path.path.segments {
                collect_path_argument_type_paths(&segment.arguments, out);
            }
        }
        syn::Type::Ptr(ptr) => collect_type_paths(&ptr.elem, out),
        syn::Type::Reference(reference) => collect_type_paths(&reference.elem, out),
        syn::Type::Slice(slice) => collect_type_paths(&slice.elem, out),
        syn::Type::TraitObject(trait_object) => {
            for bound in &trait_object.bounds {
                collect_bound_paths(bound, out);
            }
        }
        syn::Type::Tuple(tuple) => {
            for elem in &tuple.elems {
                collect_type_paths(elem, out);
            }
        }
        _ => {}
    }
}

pub(super) fn collect_path_argument_type_paths(args: &syn::PathArguments, out: &mut Vec<String>) {
    match args {
        syn::PathArguments::None => {}
        syn::PathArguments::AngleBracketed(args) => collect_angle_bracketed_args(args, out),
        syn::PathArguments::Parenthesized(args) => collect_parenthesized_args(args, out),
    }
}

fn collect_angle_bracketed_args(args: &syn::AngleBracketedGenericArguments, out: &mut Vec<String>) {
    for arg in &args.args {
        collect_generic_argument(arg, out);
    }
}

fn collect_generic_argument(arg: &syn::GenericArgument, out: &mut Vec<String>) {
    match arg {
        syn::GenericArgument::Type(ty) => collect_type_paths(ty, out),
        syn::GenericArgument::AssocType(assoc) => collect_type_paths(&assoc.ty, out),
        syn::GenericArgument::Constraint(constraint) => collect_constraint_bounds(constraint, out),
        _ => {}
    }
}

fn collect_constraint_bounds(constraint: &syn::Constraint, out: &mut Vec<String>) {
    for bound in &constraint.bounds {
        collect_bound_paths(bound, out);
    }
}

fn collect_parenthesized_args(args: &syn::ParenthesizedGenericArguments, out: &mut Vec<String>) {
    for input in &args.inputs {
        collect_type_paths(input, out);
    }
    if let syn::ReturnType::Type(_, ty) = &args.output {
        collect_type_paths(ty, out);
    }
}

fn collect_bound_paths(bound: &syn::TypeParamBound, out: &mut Vec<String>) {
    let syn::TypeParamBound::Trait(trait_bound) = bound else {
        return;
    };
    out.push(path_to_string(&trait_bound.path));
    for segment in &trait_bound.path.segments {
        collect_path_argument_type_paths(&segment.arguments, out);
    }
}

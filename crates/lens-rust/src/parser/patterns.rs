//! Pattern walk: extract bound identifier names from `syn::Pat` trees.
//!
//! Used by signature analysis to surface the parameter names a function
//! introduces, looking through references, tuples, struct destructures,
//! tuple-structs, slices, and type ascriptions.

pub(super) fn collect_pattern_names(pat: &syn::Pat, out: &mut Vec<String>) {
    match pat {
        syn::Pat::Ident(ident) => out.push(ident.ident.to_string()),
        syn::Pat::Reference(reference) => collect_pattern_names(&reference.pat, out),
        syn::Pat::Tuple(tuple) => {
            for elem in &tuple.elems {
                collect_pattern_names(elem, out);
            }
        }
        syn::Pat::TupleStruct(tuple_struct) => {
            for elem in &tuple_struct.elems {
                collect_pattern_names(elem, out);
            }
        }
        syn::Pat::Struct(pat_struct) => {
            for field in &pat_struct.fields {
                collect_pattern_names(&field.pat, out);
            }
        }
        syn::Pat::Slice(slice) => {
            for elem in &slice.elems {
                collect_pattern_names(elem, out);
            }
        }
        syn::Pat::Type(pat_type) => collect_pattern_names(&pat_type.pat, out),
        _ => {}
    }
}

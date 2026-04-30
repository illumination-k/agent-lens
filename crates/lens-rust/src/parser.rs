//! syn-based implementation of [`lens_domain::LanguageParser`] for Rust.

use lens_domain::{
    FunctionDef, FunctionSignature, LanguageParseError, LanguageParser, ReceiverShape, TreeNode,
    qualify as qualify_name,
};
use quote::ToTokens;
use syn::spanned::Spanned;

use crate::common::{WalkOptions, walk_fn_items};

/// A Rust-language parser backed by [`syn`].
///
/// Stateless; all work happens inside [`LanguageParser::parse`] and
/// [`LanguageParser::extract_functions`]. The struct exists so that
/// callers can swap in a tree-sitter backend later without changing
/// downstream code.
#[derive(Debug, Default, Clone, Copy)]
pub struct RustParser;

impl RustParser {
    pub fn new() -> Self {
        Self
    }
}

/// A Rust function annotated with its lexical module context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustFunctionDef {
    /// The language-agnostic function payload used by existing analyzers.
    pub function: FunctionDef,
    /// Bare function or method name as written in the signature.
    pub name: String,
    /// Absolute lexical name rooted at `crate`, e.g. `crate::a::parse`
    /// or `crate::a::Service::handle`.
    pub qualified_name: String,
    /// Absolute lexical module path rooted at `crate`.
    pub module: String,
    /// `impl` self-type or trait name for methods, `None` for free functions.
    pub impl_owner: Option<String>,
}

/// Parse failures surfaced by [`RustParser`].
#[derive(Debug, thiserror::Error)]
pub enum RustParseError {
    #[error("failed to parse Rust source: {0}")]
    Syn(#[from] syn::Error),
}

/// Extract functions while preserving the caller-provided file module as
/// the base for inline modules.
pub fn extract_functions_with_modules(
    source: &str,
    base_module: &str,
) -> Result<Vec<RustFunctionDef>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    extract_module_functions(&file.items, base_module, false, &mut out);
    Ok(out)
}

impl LanguageParser for RustParser {
    fn language(&self) -> &'static str {
        "rust"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, LanguageParseError> {
        let file = syn::parse_file(source)
            .map_err(RustParseError::from)
            .map_err(|err| LanguageParseError::new(self.language(), err))?;
        Ok(file_tree(&file))
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, LanguageParseError> {
        extract_with(source, WalkOptions::default())
            .map_err(|err| LanguageParseError::new(self.language(), err))
    }
}

fn extract_with(source: &str, opts: WalkOptions) -> Result<Vec<FunctionDef>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    walk_fn_items(&file.items, opts, &mut |site| {
        out.push(FunctionDef {
            name: qualify_name(site.owner, &site.sig.ident.to_string()),
            start_line: site.sig.span().start().line,
            end_line: site.block.span().end().line,
            is_test: site.is_test,
            signature: Some(signature_info(site.sig)),
            tree: function_tree(site.sig, site.block),
        });
    });
    Ok(out)
}

fn extract_module_functions(
    items: &[syn::Item],
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    for item in items {
        extract_item_functions(item, module, in_test_context, out);
    }
}

fn extract_item_functions(
    item: &syn::Item,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    match item {
        syn::Item::Fn(item_fn) => {
            let name = item_fn.sig.ident.to_string();
            let function = FunctionDef {
                name: name.clone(),
                start_line: item_fn.sig.span().start().line,
                end_line: item_fn.block.span().end().line,
                is_test: in_test_context || crate::attrs::is_test_function(&item_fn.attrs),
                signature: Some(signature_info(&item_fn.sig)),
                tree: function_tree(&item_fn.sig, &item_fn.block),
            };
            out.push(RustFunctionDef {
                function,
                qualified_name: qualify_module(module, &name),
                module: module.to_owned(),
                impl_owner: None,
                name,
            });
        }
        syn::Item::Impl(item_impl) => {
            let item_is_test = crate::attrs::has_cfg_test(&item_impl.attrs);
            extract_impl_functions(item_impl, module, in_test_context || item_is_test, out);
        }
        syn::Item::Trait(item_trait) => {
            let item_is_test = crate::attrs::has_cfg_test(&item_trait.attrs);
            extract_trait_functions(item_trait, module, in_test_context || item_is_test, out);
        }
        syn::Item::Mod(item_mod) => {
            if let Some((_, nested_items)) = &item_mod.content {
                let nested_module = qualify_module(module, &item_mod.ident.to_string());
                let item_is_test = crate::attrs::has_cfg_test(&item_mod.attrs);
                extract_module_functions(
                    nested_items,
                    &nested_module,
                    in_test_context || item_is_test,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn extract_impl_functions(
    item_impl: &syn::ItemImpl,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    let owner = crate::common::type_path_last_ident(&item_impl.self_ty);
    for impl_item in &item_impl.items {
        let syn::ImplItem::Fn(method) = impl_item else {
            continue;
        };
        let name = method.sig.ident.to_string();
        let function_name = qualify_name(owner.as_deref(), &name);
        let qualified_name = owner.as_ref().map_or_else(
            || qualify_module(module, &name),
            |owner| qualify_module(module, &format!("{owner}::{name}")),
        );
        let function = FunctionDef {
            name: function_name,
            start_line: method.sig.span().start().line,
            end_line: method.block.span().end().line,
            is_test: in_test_context || crate::attrs::is_test_function(&method.attrs),
            signature: Some(signature_info(&method.sig)),
            tree: function_tree(&method.sig, &method.block),
        };
        out.push(RustFunctionDef {
            function,
            qualified_name,
            module: module.to_owned(),
            impl_owner: owner.clone(),
            name,
        });
    }
}

fn extract_trait_functions(
    item_trait: &syn::ItemTrait,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    let owner = item_trait.ident.to_string();
    for trait_item in &item_trait.items {
        let syn::TraitItem::Fn(method) = trait_item else {
            continue;
        };
        let Some(block) = &method.default else {
            continue;
        };
        let name = method.sig.ident.to_string();
        let function = FunctionDef {
            name: qualify_name(Some(&owner), &name),
            start_line: method.sig.span().start().line,
            end_line: block.span().end().line,
            is_test: in_test_context || crate::attrs::is_test_function(&method.attrs),
            signature: Some(signature_info(&method.sig)),
            tree: function_tree(&method.sig, block),
        };
        out.push(RustFunctionDef {
            function,
            qualified_name: qualify_module(module, &format!("{owner}::{name}")),
            module: module.to_owned(),
            impl_owner: Some(owner.clone()),
            name,
        });
    }
}

fn qualify_module(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

fn file_tree(file: &syn::File) -> TreeNode {
    TreeNode::with_children("File", "", file.items.iter().map(item_tree).collect())
}

fn item_tree(item: &syn::Item) -> TreeNode {
    match item {
        syn::Item::Fn(item_fn) => function_tree(&item_fn.sig, &item_fn.block),
        syn::Item::Impl(item_impl) => TreeNode::with_children(
            type_label("Impl", &item_impl.self_ty),
            "",
            item_impl
                .items
                .iter()
                .filter_map(|item| match item {
                    syn::ImplItem::Fn(method) => Some(function_tree(&method.sig, &method.block)),
                    _ => None,
                })
                .collect(),
        ),
        syn::Item::Mod(item_mod) => TreeNode::with_children(
            format!("Mod({})", item_mod.ident),
            "",
            item_mod
                .content
                .as_ref()
                .map(|(_, items)| items.iter().map(item_tree).collect())
                .unwrap_or_default(),
        ),
        syn::Item::Trait(item_trait) => TreeNode::with_children(
            format!("Trait({})", item_trait.ident),
            "",
            item_trait
                .items
                .iter()
                .filter_map(|item| match item {
                    syn::TraitItem::Fn(method) => method
                        .default
                        .as_ref()
                        .map(|block| function_tree(&method.sig, block)),
                    _ => None,
                })
                .collect(),
        ),
        syn::Item::Struct(item_struct) => TreeNode::leaf(format!("Struct({})", item_struct.ident)),
        syn::Item::Enum(item_enum) => TreeNode::leaf(format!("Enum({})", item_enum.ident)),
        syn::Item::Use(_) => TreeNode::leaf("Use"),
        syn::Item::Const(item_const) => type_node("Const", &item_const.ty),
        syn::Item::Static(item_static) => type_node("Static", &item_static.ty),
        syn::Item::Type(item_type) => type_node("TypeAlias", &item_type.ty),
        _ => TreeNode::leaf(item_fallback_label(item)),
    }
}

fn function_tree(sig: &syn::Signature, block: &syn::Block) -> TreeNode {
    TreeNode::with_children("Function", "", vec![signature_tree(sig), block_tree(block)])
}

fn signature_tree(sig: &syn::Signature) -> TreeNode {
    let mut children = Vec::new();
    if sig.asyncness.is_some() {
        children.push(TreeNode::leaf("Async"));
    }
    if sig.constness.is_some() {
        children.push(TreeNode::leaf("Const"));
    }
    for input in &sig.inputs {
        children.push(param_tree(input));
    }
    children.push(return_type_tree(&sig.output));
    TreeNode::with_children("FnSignature", "", children)
}

fn signature_info(sig: &syn::Signature) -> FunctionSignature {
    let mut parameter_names = Vec::new();
    let mut parameter_type_paths = Vec::new();
    let mut parameter_count = 0usize;
    let mut receiver = ReceiverShape::None;

    for input in &sig.inputs {
        match input {
            syn::FnArg::Receiver(recv) => {
                receiver = receiver_shape(recv);
            }
            syn::FnArg::Typed(pat_type) => {
                parameter_count += 1;
                collect_pattern_names(&pat_type.pat, &mut parameter_names);
                collect_type_paths(&pat_type.ty, &mut parameter_type_paths);
            }
        }
    }

    let mut return_type_paths = Vec::new();
    if let syn::ReturnType::Type(_, ty) = &sig.output {
        collect_type_paths(ty, &mut return_type_paths);
    }

    FunctionSignature {
        name_tokens: identifier_tokens(&sig.ident.to_string()),
        parameter_count,
        parameter_names,
        parameter_type_paths,
        return_type_paths,
        generics: generic_summaries(&sig.generics),
        receiver,
    }
}

fn receiver_shape(receiver: &syn::Receiver) -> ReceiverShape {
    match (&receiver.reference, &receiver.mutability) {
        (Some(_), Some(_)) => ReceiverShape::RefMut,
        (Some(_), None) => ReceiverShape::Ref,
        (None, _) => ReceiverShape::Value,
    }
}

fn collect_pattern_names(pat: &syn::Pat, out: &mut Vec<String>) {
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

fn collect_type_paths(ty: &syn::Type, out: &mut Vec<String>) {
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

fn collect_path_argument_type_paths(args: &syn::PathArguments, out: &mut Vec<String>) {
    match args {
        syn::PathArguments::None => {}
        syn::PathArguments::AngleBracketed(args) => {
            for arg in &args.args {
                match arg {
                    syn::GenericArgument::Type(ty) => collect_type_paths(ty, out),
                    syn::GenericArgument::AssocType(assoc) => collect_type_paths(&assoc.ty, out),
                    syn::GenericArgument::Constraint(constraint) => {
                        for bound in &constraint.bounds {
                            collect_bound_paths(bound, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        syn::PathArguments::Parenthesized(args) => {
            for input in &args.inputs {
                collect_type_paths(input, out);
            }
            if let syn::ReturnType::Type(_, ty) = &args.output {
                collect_type_paths(ty, out);
            }
        }
    }
}

fn collect_bound_paths(bound: &syn::TypeParamBound, out: &mut Vec<String>) {
    if let syn::TypeParamBound::Trait(trait_bound) = bound {
        out.push(path_to_string(&trait_bound.path));
        for segment in &trait_bound.path.segments {
            collect_path_argument_type_paths(&segment.arguments, out);
        }
    }
}

fn generic_summaries(generics: &syn::Generics) -> Vec<String> {
    let mut out: Vec<String> = generics.params.iter().map(normalized_tokens).collect();
    if let Some(where_clause) = &generics.where_clause {
        out.extend(where_clause.predicates.iter().map(normalized_tokens));
    }
    out.retain(|item| !item.is_empty());
    out
}

fn identifier_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_is_lower_or_digit = false;
    for ch in name.chars() {
        if ch == '_' || !ch.is_alphanumeric() {
            push_identifier_token(&mut tokens, &mut current);
            prev_is_lower_or_digit = false;
            continue;
        }
        if ch.is_uppercase() && prev_is_lower_or_digit {
            push_identifier_token(&mut tokens, &mut current);
        }
        current.extend(ch.to_lowercase());
        prev_is_lower_or_digit = ch.is_lowercase() || ch.is_ascii_digit();
    }
    push_identifier_token(&mut tokens, &mut current);
    tokens
}

fn push_identifier_token(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

fn param_tree(arg: &syn::FnArg) -> TreeNode {
    match arg {
        syn::FnArg::Receiver(receiver) => {
            let label = if receiver.reference.is_some() {
                "ReceiverRef"
            } else {
                "Receiver"
            };
            TreeNode::leaf(label)
        }
        syn::FnArg::Typed(pat_type) => TreeNode::with_children(
            type_label("Param", &pat_type.ty),
            "",
            vec![pat_tree(&pat_type.pat), type_tree(&pat_type.ty)],
        ),
    }
}

fn return_type_tree(output: &syn::ReturnType) -> TreeNode {
    match output {
        syn::ReturnType::Default => TreeNode::leaf("ReturnType(())"),
        syn::ReturnType::Type(_, ty) => {
            TreeNode::with_children(type_label("ReturnType", ty), "", vec![type_tree(ty)])
        }
    }
}

fn block_tree(block: &syn::Block) -> TreeNode {
    TreeNode::with_children("Block", "", block.stmts.iter().map(stmt_tree).collect())
}

fn stmt_tree(stmt: &syn::Stmt) -> TreeNode {
    match stmt {
        syn::Stmt::Local(local) => {
            let mut children = vec![pat_tree(&local.pat)];
            if let Some(init) = &local.init {
                children.push(expr_tree(&init.expr));
                if let Some((_, diverge)) = &init.diverge {
                    children.push(TreeNode::with_children(
                        "LetElse",
                        "",
                        vec![expr_tree(diverge)],
                    ));
                }
            }
            TreeNode::with_children("Let", "", children)
        }
        syn::Stmt::Item(item) => item_tree(item),
        syn::Stmt::Expr(expr, semi) => {
            let node = expr_tree(expr);
            if semi.is_some() {
                TreeNode::with_children("ExprStmt", "", vec![node])
            } else {
                node
            }
        }
        syn::Stmt::Macro(stmt_macro) => TreeNode::leaf(format!(
            "MacroStmt({})",
            path_to_string(&stmt_macro.mac.path)
        )),
    }
}

fn expr_tree(expr: &syn::Expr) -> TreeNode {
    match expr {
        syn::Expr::Array(array) => {
            TreeNode::with_children("Array", "", array.elems.iter().map(expr_tree).collect())
        }
        syn::Expr::Assign(assign) => TreeNode::with_children(
            "Assign",
            "",
            vec![expr_tree(&assign.left), expr_tree(&assign.right)],
        ),
        syn::Expr::Async(async_expr) => {
            TreeNode::with_children("AsyncBlock", "", vec![block_tree(&async_expr.block)])
        }
        syn::Expr::Await(await_expr) => {
            TreeNode::with_children("Await", "", vec![expr_tree(&await_expr.base)])
        }
        syn::Expr::Binary(binary) => TreeNode::with_children(
            format!("Binary({})", normalized_tokens(&binary.op)),
            "",
            vec![expr_tree(&binary.left), expr_tree(&binary.right)],
        ),
        syn::Expr::Block(block) => block_tree(&block.block),
        syn::Expr::Break(break_expr) => optional_expr("Break", break_expr.expr.as_deref()),
        syn::Expr::Call(call) => call_tree(call),
        syn::Expr::Cast(cast) => TreeNode::with_children(
            type_label("Cast", &cast.ty),
            "",
            vec![expr_tree(&cast.expr), type_tree(&cast.ty)],
        ),
        syn::Expr::Closure(closure) => closure_tree(closure),
        syn::Expr::Const(const_expr) => {
            TreeNode::with_children("ConstBlock", "", vec![block_tree(&const_expr.block)])
        }
        syn::Expr::Continue(_) => TreeNode::leaf("Continue"),
        syn::Expr::Field(field) => TreeNode::with_children(
            format!("FieldAccess({})", member_to_string(&field.member)),
            "",
            vec![expr_tree(&field.base)],
        ),
        syn::Expr::ForLoop(for_loop) => TreeNode::with_children(
            "For",
            "",
            vec![
                pat_tree(&for_loop.pat),
                expr_tree(&for_loop.expr),
                block_tree(&for_loop.body),
            ],
        ),
        syn::Expr::Group(group) => expr_tree(&group.expr),
        syn::Expr::If(if_expr) => {
            let mut children = vec![expr_tree(&if_expr.cond), block_tree(&if_expr.then_branch)];
            if let Some((_, else_expr)) = &if_expr.else_branch {
                children.push(TreeNode::with_children(
                    "Else",
                    "",
                    vec![expr_tree(else_expr)],
                ));
            }
            TreeNode::with_children("If", "", children)
        }
        syn::Expr::Index(index) => TreeNode::with_children(
            "Index",
            "",
            vec![expr_tree(&index.expr), expr_tree(&index.index)],
        ),
        syn::Expr::Infer(_) => TreeNode::leaf("Infer"),
        syn::Expr::Let(let_expr) => TreeNode::with_children(
            "LetExpr",
            "",
            vec![pat_tree(&let_expr.pat), expr_tree(&let_expr.expr)],
        ),
        syn::Expr::Lit(lit) => TreeNode::leaf(lit_label(&lit.lit)),
        syn::Expr::Loop(loop_expr) => {
            TreeNode::with_children("Loop", "", vec![block_tree(&loop_expr.body)])
        }
        syn::Expr::Macro(mac) => {
            TreeNode::leaf(format!("MacroCall({})", path_to_string(&mac.mac.path)))
        }
        syn::Expr::Match(match_expr) => {
            let mut children = vec![expr_tree(&match_expr.expr)];
            children.extend(match_expr.arms.iter().map(arm_tree));
            TreeNode::with_children("Match", "", children)
        }
        syn::Expr::MethodCall(method_call) => TreeNode::with_children(
            format!("MethodCall({})", method_call.method),
            "",
            std::iter::once(expr_tree(&method_call.receiver))
                .chain(method_call.args.iter().map(expr_tree))
                .collect(),
        ),
        syn::Expr::Paren(paren) => expr_tree(&paren.expr),
        syn::Expr::Path(path) => TreeNode::leaf(format!("Path({})", path_to_string(&path.path))),
        syn::Expr::Range(range) => TreeNode::with_children(
            "Range",
            "",
            range
                .start
                .iter()
                .chain(range.end.iter())
                .map(|expr| expr_tree(expr))
                .collect(),
        ),
        syn::Expr::Reference(reference) => {
            TreeNode::with_children("Reference", "", vec![expr_tree(&reference.expr)])
        }
        syn::Expr::Repeat(repeat) => TreeNode::with_children(
            "Repeat",
            "",
            vec![expr_tree(&repeat.expr), expr_tree(&repeat.len)],
        ),
        syn::Expr::Return(return_expr) => optional_expr("Return", return_expr.expr.as_deref()),
        syn::Expr::Struct(struct_expr) => struct_init_tree(struct_expr),
        syn::Expr::Try(try_expr) => {
            TreeNode::with_children("Try", "", vec![expr_tree(&try_expr.expr)])
        }
        syn::Expr::TryBlock(try_block) => {
            TreeNode::with_children("TryBlock", "", vec![block_tree(&try_block.block)])
        }
        syn::Expr::Tuple(tuple) => {
            TreeNode::with_children("Tuple", "", tuple.elems.iter().map(expr_tree).collect())
        }
        syn::Expr::Unary(unary) => TreeNode::with_children(
            format!("Unary({})", normalized_tokens(&unary.op)),
            "",
            vec![expr_tree(&unary.expr)],
        ),
        syn::Expr::Unsafe(unsafe_expr) => {
            TreeNode::with_children("UnsafeBlock", "", vec![block_tree(&unsafe_expr.block)])
        }
        syn::Expr::While(while_expr) => TreeNode::with_children(
            "While",
            "",
            vec![expr_tree(&while_expr.cond), block_tree(&while_expr.body)],
        ),
        syn::Expr::Yield(yield_expr) => optional_expr("Yield", yield_expr.expr.as_deref()),
        _ => TreeNode::leaf(expr_fallback_label(expr)),
    }
}

fn call_tree(call: &syn::ExprCall) -> TreeNode {
    let (label, mut children) = match call.func.as_ref() {
        syn::Expr::Path(path) => (
            format!("CallPath({})", path_to_string(&path.path)),
            Vec::new(),
        ),
        callee => ("Call".to_owned(), vec![expr_tree(callee)]),
    };
    children.extend(call.args.iter().map(expr_tree));
    TreeNode::with_children(label, "", children)
}

fn closure_tree(closure: &syn::ExprClosure) -> TreeNode {
    let mut children: Vec<_> = closure.inputs.iter().map(pat_tree).collect();
    match &closure.output {
        syn::ReturnType::Default => {}
        syn::ReturnType::Type(_, _) => children.push(return_type_tree(&closure.output)),
    }
    children.push(expr_tree(&closure.body));
    TreeNode::with_children("Closure", "", children)
}

fn arm_tree(arm: &syn::Arm) -> TreeNode {
    let mut children = vec![pat_tree(&arm.pat)];
    if let Some((_, guard)) = &arm.guard {
        children.push(TreeNode::with_children("Guard", "", vec![expr_tree(guard)]));
    }
    children.push(expr_tree(&arm.body));
    TreeNode::with_children("MatchArm", "", children)
}

fn struct_init_tree(struct_expr: &syn::ExprStruct) -> TreeNode {
    let mut children: Vec<_> = struct_expr
        .fields
        .iter()
        .map(|field| {
            TreeNode::with_children(
                format!("FieldInit({})", member_to_string(&field.member)),
                "",
                vec![expr_tree(&field.expr)],
            )
        })
        .collect();
    if let Some(rest) = &struct_expr.rest {
        children.push(TreeNode::with_children(
            "StructRest",
            "",
            vec![expr_tree(rest)],
        ));
    }
    TreeNode::with_children(
        format!("StructInit({})", path_to_string(&struct_expr.path)),
        "",
        children,
    )
}

fn type_tree(ty: &syn::Type) -> TreeNode {
    match ty {
        syn::Type::Array(array) => TreeNode::with_children(
            "TypeArray",
            "",
            vec![type_tree(&array.elem), expr_tree(&array.len)],
        ),
        syn::Type::BareFn(bare_fn) => TreeNode::with_children(
            "TypeBareFn",
            "",
            bare_fn
                .inputs
                .iter()
                .map(|arg| type_tree(&arg.ty))
                .collect(),
        ),
        syn::Type::Group(group) => type_tree(&group.elem),
        syn::Type::ImplTrait(impl_trait) => TreeNode::with_children(
            "TypeImplTrait",
            "",
            impl_trait
                .bounds
                .iter()
                .map(type_param_bound_tree)
                .collect(),
        ),
        syn::Type::Infer(_) => TreeNode::leaf("TypeInfer"),
        syn::Type::Macro(mac) => {
            TreeNode::leaf(format!("TypeMacro({})", path_to_string(&mac.mac.path)))
        }
        syn::Type::Never(_) => TreeNode::leaf("TypeNever"),
        syn::Type::Paren(paren) => type_tree(&paren.elem),
        syn::Type::Path(type_path) => type_path_tree(type_path),
        syn::Type::Ptr(ptr) => TreeNode::with_children("TypePtr", "", vec![type_tree(&ptr.elem)]),
        syn::Type::Reference(reference) => TreeNode::with_children(
            if reference.mutability.is_some() {
                "TypeRefMut"
            } else {
                "TypeRef"
            },
            "",
            vec![type_tree(&reference.elem)],
        ),
        syn::Type::Slice(slice) => {
            TreeNode::with_children("TypeSlice", "", vec![type_tree(&slice.elem)])
        }
        syn::Type::TraitObject(trait_object) => TreeNode::with_children(
            "TypeTraitObject",
            "",
            trait_object
                .bounds
                .iter()
                .map(type_param_bound_tree)
                .collect(),
        ),
        syn::Type::Tuple(tuple) => {
            TreeNode::with_children("TypeTuple", "", tuple.elems.iter().map(type_tree).collect())
        }
        _ => TreeNode::leaf(type_label("TypeOther", ty)),
    }
}

fn type_path_tree(type_path: &syn::TypePath) -> TreeNode {
    let children = type_path
        .path
        .segments
        .iter()
        .flat_map(segment_generic_trees)
        .collect();
    TreeNode::with_children(
        format!("TypePath({})", path_to_string(&type_path.path)),
        "",
        children,
    )
}

fn segment_generic_trees(segment: &syn::PathSegment) -> Vec<TreeNode> {
    match &segment.arguments {
        syn::PathArguments::None => Vec::new(),
        syn::PathArguments::AngleBracketed(args) => args
            .args
            .iter()
            .filter_map(|arg| match arg {
                syn::GenericArgument::Type(ty) => Some(type_tree(ty)),
                syn::GenericArgument::AssocType(assoc) => Some(TreeNode::with_children(
                    format!("AssocType({})", assoc.ident),
                    "",
                    vec![type_tree(&assoc.ty)],
                )),
                syn::GenericArgument::Const(expr) => Some(expr_tree(expr)),
                syn::GenericArgument::Lifetime(_) => Some(TreeNode::leaf("Lifetime")),
                _ => None,
            })
            .collect(),
        syn::PathArguments::Parenthesized(args) => args
            .inputs
            .iter()
            .map(type_tree)
            .chain(match &args.output {
                syn::ReturnType::Default => None,
                syn::ReturnType::Type(_, _) => Some(return_type_tree(&args.output)),
            })
            .collect(),
    }
}

fn type_param_bound_tree(bound: &syn::TypeParamBound) -> TreeNode {
    match bound {
        syn::TypeParamBound::Trait(trait_bound) => TreeNode::with_children(
            format!("TraitBound({})", path_to_string(&trait_bound.path)),
            "",
            trait_bound
                .path
                .segments
                .iter()
                .flat_map(segment_generic_trees)
                .collect(),
        ),
        syn::TypeParamBound::Lifetime(_) => TreeNode::leaf("LifetimeBound"),
        _ => TreeNode::leaf("TypeParamBound"),
    }
}

fn pat_tree(pat: &syn::Pat) -> TreeNode {
    match pat {
        syn::Pat::Const(_) => TreeNode::leaf("PatConst"),
        syn::Pat::Ident(ident) => {
            if ident.mutability.is_some() {
                TreeNode::leaf("PatIdentMut")
            } else {
                TreeNode::leaf("PatIdent")
            }
        }
        syn::Pat::Lit(lit) => TreeNode::leaf(lit_label(&lit.lit)),
        syn::Pat::Macro(mac) => {
            TreeNode::leaf(format!("PatMacro({})", path_to_string(&mac.mac.path)))
        }
        syn::Pat::Or(or) => {
            TreeNode::with_children("PatOr", "", or.cases.iter().map(pat_tree).collect())
        }
        syn::Pat::Paren(paren) => pat_tree(&paren.pat),
        syn::Pat::Path(path) => TreeNode::leaf(format!("PatPath({})", path_to_string(&path.path))),
        syn::Pat::Range(range) => TreeNode::with_children(
            "PatRange",
            "",
            range
                .start
                .iter()
                .chain(range.end.iter())
                .map(|expr| expr_tree(expr))
                .collect(),
        ),
        syn::Pat::Reference(reference) => {
            TreeNode::with_children("PatReference", "", vec![pat_tree(&reference.pat)])
        }
        syn::Pat::Rest(_) => TreeNode::leaf("PatRest"),
        syn::Pat::Slice(slice) => {
            TreeNode::with_children("PatSlice", "", slice.elems.iter().map(pat_tree).collect())
        }
        syn::Pat::Struct(pat_struct) => TreeNode::with_children(
            format!("PatStruct({})", path_to_string(&pat_struct.path)),
            "",
            pat_struct
                .fields
                .iter()
                .map(|field| {
                    TreeNode::with_children(
                        format!("PatField({})", member_to_string(&field.member)),
                        "",
                        vec![pat_tree(&field.pat)],
                    )
                })
                .collect(),
        ),
        syn::Pat::Tuple(tuple) => {
            TreeNode::with_children("PatTuple", "", tuple.elems.iter().map(pat_tree).collect())
        }
        syn::Pat::TupleStruct(tuple_struct) => TreeNode::with_children(
            format!("PatTupleStruct({})", path_to_string(&tuple_struct.path)),
            "",
            tuple_struct.elems.iter().map(pat_tree).collect(),
        ),
        syn::Pat::Type(pat_type) => TreeNode::with_children(
            type_label("PatType", &pat_type.ty),
            "",
            vec![pat_tree(&pat_type.pat), type_tree(&pat_type.ty)],
        ),
        syn::Pat::Verbatim(_) => TreeNode::leaf("PatVerbatim"),
        syn::Pat::Wild(_) => TreeNode::leaf("PatWild"),
        _ => TreeNode::leaf("Pat"),
    }
}

fn type_node(prefix: &str, ty: &syn::Type) -> TreeNode {
    TreeNode::with_children(type_label(prefix, ty), "", vec![type_tree(ty)])
}

fn type_label(prefix: &str, ty: &syn::Type) -> String {
    format!("{prefix}({})", type_summary(ty))
}

fn type_summary(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Array(array) => format!("[{}]", type_summary(&array.elem)),
        syn::Type::BareFn(_) => "fn".to_owned(),
        syn::Type::Group(group) => type_summary(&group.elem),
        syn::Type::ImplTrait(_) => "impl Trait".to_owned(),
        syn::Type::Infer(_) => "_".to_owned(),
        syn::Type::Macro(mac) => path_to_string(&mac.mac.path),
        syn::Type::Never(_) => "!".to_owned(),
        syn::Type::Paren(paren) => type_summary(&paren.elem),
        syn::Type::Path(type_path) => path_to_string(&type_path.path),
        syn::Type::Ptr(ptr) => format!("*{}", type_summary(&ptr.elem)),
        syn::Type::Reference(reference) => {
            if reference.mutability.is_some() {
                format!("&mut {}", type_summary(&reference.elem))
            } else {
                format!("&{}", type_summary(&reference.elem))
            }
        }
        syn::Type::Slice(slice) => format!("[{}]", type_summary(&slice.elem)),
        syn::Type::TraitObject(_) => "dyn Trait".to_owned(),
        syn::Type::Tuple(tuple) => format!(
            "({})",
            tuple
                .elems
                .iter()
                .map(type_summary)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => normalized_tokens(ty),
    }
}

fn optional_expr(label: &'static str, expr: Option<&syn::Expr>) -> TreeNode {
    TreeNode::with_children(label, "", expr.into_iter().map(expr_tree).collect())
}

fn lit_label(lit: &syn::Lit) -> &'static str {
    match lit {
        syn::Lit::Str(_) => "LitStr",
        syn::Lit::ByteStr(_) => "LitByteStr",
        syn::Lit::Byte(_) => "LitByte",
        syn::Lit::Char(_) => "LitChar",
        syn::Lit::Int(_) => "LitInt",
        syn::Lit::Float(_) => "LitFloat",
        syn::Lit::Bool(_) => "LitBool",
        _ => "Lit",
    }
}

fn path_to_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn member_to_string(member: &syn::Member) -> String {
    match member {
        syn::Member::Named(ident) => ident.to_string(),
        syn::Member::Unnamed(index) => index.index.to_string(),
    }
}

fn item_fallback_label(item: &syn::Item) -> String {
    format!("Item({})", normalized_tokens(item))
}

fn expr_fallback_label(expr: &syn::Expr) -> String {
    format!("Expr({})", normalized_tokens(expr))
}

fn normalized_tokens(tokens: &impl ToTokens) -> String {
    tokens
        .to_token_stream()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::{TSEDOptions, calculate_tsed, find_similar_functions};
    use rstest::rstest;
    use syn::parse::Parser;

    fn parse_functions(src: &str) -> Vec<FunctionDef> {
        let mut parser = RustParser::new();
        parser.extract_functions(src).unwrap()
    }

    fn has_label(tree: &TreeNode, label: &str) -> bool {
        tree.label == label || tree.children.iter().any(|child| has_label(child, label))
    }

    fn count_label(tree: &TreeNode, label: &str) -> usize {
        usize::from(tree.label == label)
            + tree
                .children
                .iter()
                .map(|child| count_label(child, label))
                .sum::<usize>()
    }

    fn assert_expr_has_labels(src: &str, expected: &[&str]) {
        let expr: syn::Expr = syn::parse_str(src).unwrap();
        let tree = expr_tree(&expr);
        for label in expected {
            assert!(
                has_label(&tree, label),
                "missing expr label {label} in {src}: {tree:?}"
            );
        }
    }

    fn assert_type_has_labels(src: &str, expected: &[&str]) {
        let ty: syn::Type = syn::parse_str(src).unwrap_or_else(|err| {
            panic!("failed to parse type fixture {src:?}: {err}");
        });
        let tree = type_tree(&ty);
        for label in expected {
            assert!(
                has_label(&tree, label),
                "missing type label {label} in {src}: {tree:?}"
            );
        }
    }

    fn assert_pat_has_labels(src: &str, expected: &[&str]) {
        let pat = syn::Pat::parse_single
            .parse_str(src)
            .or_else(|_| syn::Pat::parse_multi.parse_str(src))
            .unwrap_or_else(|err| {
                panic!("failed to parse pattern fixture {src:?}: {err}");
            });
        let tree = pat_tree(&pat);
        for label in expected {
            assert!(
                has_label(&tree, label),
                "missing pat label {label} in {src}: {tree:?}"
            );
        }
    }

    #[test]
    fn extracts_free_function_name_and_lines() {
        let src = "fn first() {}\nfn second() { let _x = 1; }\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 2);
        assert_eq!(funcs[0].name, "first");
        assert_eq!(funcs[1].name, "second");
        assert_eq!(funcs[0].start_line, 1);
        assert_eq!(funcs[0].end_line, 1);
        assert_eq!(funcs[1].start_line, 2);
        assert_eq!(funcs[1].end_line, 2);
    }

    #[test]
    fn end_line_tracks_closing_brace_for_multi_line_function() {
        let src = "fn body() {\n    let x = 1;\n    let y = 2;\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].start_line, 1);
        assert_eq!(funcs[0].end_line, 4);
    }

    #[test]
    fn language_identifier_is_rust() {
        let parser = RustParser::new();
        assert_eq!(parser.language(), "rust");
    }

    #[test]
    fn parse_error_exposes_underlying_syn_error_via_source() {
        let mut parser = RustParser::new();
        let err = parser.parse("fn ??? {").unwrap_err();
        let source = std::error::Error::source(&err).expect("source should be Some");
        // The underlying syn error should round-trip through Display so the
        // chained error message stays intact.
        assert!(!format!("{source}").is_empty());
    }

    #[test]
    fn extracts_impl_methods_with_qualified_names() {
        let src = r#"
struct Foo;
impl Foo {
    fn bar(&self) -> i32 { 1 }
    fn baz(&self) -> i32 { 2 }
}
"#;
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Foo::bar", "Foo::baz"]);
    }

    #[test]
    fn extracts_trait_default_methods_only() {
        let src = r#"
trait T {
    fn required(&self);
    fn with_default(&self) -> u32 { 42 }
}
"#;
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "T::with_default");
    }

    #[test]
    fn extracts_functions_inside_inline_modules() {
        let src = r#"
mod inner {
    fn hidden() -> u32 { 0 }
}
"#;
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "hidden");
    }

    #[test]
    fn module_aware_extraction_preserves_lexical_qualified_names() {
        let src = r#"
mod inner {
    fn parse() {}
    struct S;
    impl S {
        fn call(&self) {}
    }
}
"#;
        let funcs = extract_functions_with_modules(src, "crate::outer").unwrap();
        let names: Vec<_> = funcs
            .iter()
            .map(|f| {
                (
                    f.name.as_str(),
                    f.qualified_name.as_str(),
                    f.module.as_str(),
                    f.impl_owner.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                (
                    "parse",
                    "crate::outer::inner::parse",
                    "crate::outer::inner",
                    None
                ),
                (
                    "call",
                    "crate::outer::inner::S::call",
                    "crate::outer::inner",
                    Some("S"),
                ),
            ]
        );
    }

    #[test]
    fn module_aware_extraction_propagates_test_contexts() {
        let src = r#"
#[test]
fn direct_test() {}

#[cfg(test)]
mod tests {
    fn helper() {}
    struct Bag;
    impl Bag {
        fn fixture() {}
    }
    trait Harness {
        fn default_helper() {}
    }
}
"#;
        let funcs = extract_functions_with_modules(src, "crate").unwrap();
        let flags: Vec<_> = funcs
            .iter()
            .map(|f| (f.qualified_name.as_str(), f.function.is_test))
            .collect();
        assert_eq!(
            flags,
            [
                ("crate::direct_test", true),
                ("crate::tests::helper", true),
                ("crate::tests::Bag::fixture", true),
                ("crate::tests::Harness::default_helper", true),
            ]
        );
    }

    #[test]
    fn parse_returns_error_for_invalid_rust() {
        let mut parser = RustParser::new();
        let err = parser.parse("fn ??? {").unwrap_err();
        assert!(format!("{err}").contains("failed to parse Rust source"));
    }

    #[test]
    fn clones_are_detected_as_highly_similar() {
        let src = r#"
fn original(xs: &[u32]) -> u32 {
    let mut total = 0;
    for x in xs {
        total += *x;
    }
    total
}

fn cloned(ys: &[u32]) -> u32 {
    let mut sum = 0;
    for y in ys {
        sum += *y;
    }
    sum
}
"#;
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim > 0.9,
            "expected renamed clone to stay > 0.9 similar, got {sim}"
        );
    }

    #[test]
    fn structurally_different_functions_score_low() {
        let src = r#"
fn loopy(xs: &[u32]) -> u32 {
    let mut total = 0;
    for x in xs {
        total += *x;
    }
    total
}

fn recursive(n: u32) -> u32 {
    if n == 0 { 0 } else { n + recursive(n - 1) }
}
"#;
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim < 0.8,
            "expected structurally different functions to score < 0.8, got {sim}"
        );
    }

    #[test]
    fn projected_tree_preserves_signature_types_and_rust_expr_categories() {
        let src = r#"
struct User { id: UserId }
struct Repo;
struct UserId(u64);

fn build_user(id: UserId, repo: &Repo) -> User {
    let user = User { id };
    repo.save(user.id)?;
    crate::audit::log(user.id);
    user
}
"#;
        let funcs = parse_functions(src);
        let tree = &funcs[0].tree;
        for label in [
            "FnSignature",
            "Param(UserId)",
            "TypePath(UserId)",
            "Param(&Repo)",
            "ReturnType(User)",
            "StructInit(User)",
            "MethodCall(save)",
            "FieldAccess(id)",
            "Try",
            "CallPath(crate::audit::log)",
        ] {
            assert!(has_label(tree, label), "missing projected label {label}");
        }
    }

    #[test]
    fn parse_projects_top_level_item_categories() {
        let src = r#"
use crate::dep;
const LIMIT: usize = 10;
static NAME: &str = "agent";
type Alias = Option<User>;
struct User;
enum Kind { One }
fn free() {}
impl User { fn method(&self) {} }
trait Trait { fn defaulted(&self) {} }
mod nested { fn inside() {} }
extern crate alloc;
"#;
        let mut parser = RustParser::new();
        let tree = parser.parse(src).unwrap();
        for label in [
            "Use",
            "Const(usize)",
            "Static(&str)",
            "TypeAlias(Option)",
            "Struct(User)",
            "Enum(Kind)",
            "Function",
            "Impl(User)",
            "Trait(Trait)",
            "Mod(nested)",
            "Item(extern crate alloc ;)",
        ] {
            assert!(
                has_label(&tree, label),
                "missing file item label {label}: {tree:?}"
            );
        }
        assert_eq!(count_label(&tree, "Function"), 4);
    }

    #[test]
    fn expression_projection_covers_rust_expression_categories() {
        for (src, labels) in [
            ("[a, b]", &["Array", "Path(a)"][..]),
            ("target = value", &["Assign", "Path(target)", "Path(value)"]),
            ("async { value.await }", &["AsyncBlock", "Await"]),
            ("left + right", &["Binary(+)", "Path(left)", "Path(right)"]),
            ("{ value }", &["Block", "Path(value)"]),
            ("break value", &["Break", "Path(value)"]),
            ("value as u32", &["Cast(u32)", "TypePath(u32)"]),
            (
                "|x: u32| -> u32 { x }",
                &["Closure", "PatType(u32)", "ReturnType(u32)"],
            ),
            ("const { 1 }", &["ConstBlock", "LitInt"]),
            ("continue", &["Continue"]),
            ("for x in xs { x; }", &["For", "PatIdent", "Path(xs)"]),
            ("(value)", &["Path(value)"]),
            (
                "if flag { one } else { two }",
                &["If", "Else", "Path(flag)"],
            ),
            ("items[0]", &["Index", "Path(items)", "LitInt"]),
            ("_", &["Infer"]),
            (
                "let Some(x) = value",
                &["LetExpr", "PatTupleStruct(Some)", "Path(value)"],
            ),
            ("1", &["LitInt"]),
            ("loop { break; }", &["Loop", "Break"]),
            ("todo!()", &["MacroCall(todo)"]),
            (
                "match value { Some(x) if x > 0 => x, _ => 0 }",
                &["Match", "MatchArm", "Guard", "PatTupleStruct(Some)"],
            ),
            (
                "receiver.method(arg)",
                &["MethodCall(method)", "Path(receiver)", "Path(arg)"],
            ),
            ("path::to::value", &["Path(path::to::value)"]),
            ("0..10", &["Range", "LitInt"]),
            ("&value", &["Reference", "Path(value)"]),
            ("[value; 3]", &["Repeat", "Path(value)", "LitInt"]),
            ("return value", &["Return", "Path(value)"]),
            (
                "User { id, ..base }",
                &["StructInit(User)", "FieldInit(id)", "StructRest"],
            ),
            ("fallible()?", &["Try", "CallPath(fallible)"]),
            ("try { fallible()? }", &["TryBlock", "Try"]),
            ("(a, b)", &["Tuple", "Path(a)", "Path(b)"]),
            ("!flag", &["Unary(!)", "Path(flag)"]),
            ("unsafe { value }", &["UnsafeBlock", "Path(value)"]),
            ("while flag { break; }", &["While", "Path(flag)", "Break"]),
            ("yield value", &["Yield", "Path(value)"]),
        ] {
            assert_expr_has_labels(src, labels);
        }

        let verbatim = syn::Expr::Verbatim(quote::quote!(custom expr));
        assert_eq!(expr_fallback_label(&verbatim), "Expr(custom expr)");

        let grouped = syn::Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: Default::default(),
            expr: Box::new(syn::parse_str("value").unwrap()),
        });
        let grouped_tree = expr_tree(&grouped);
        assert!(has_label(&grouped_tree, "Path(value)"));
    }

    #[test]
    fn type_projection_covers_rust_type_categories() {
        for (src, labels) in [
            ("[u8; 4]", &["TypeArray", "TypePath(u8)", "LitInt"][..]),
            ("fn(u8) -> bool", &["TypeBareFn", "TypePath(u8)"]),
            (
                "impl Iterator<Item = u8>",
                &["TypeImplTrait", "TraitBound(Iterator)", "AssocType(Item)"],
            ),
            ("_", &["TypeInfer"]),
            ("ty_macro!()", &["TypeMacro(ty_macro)"]),
            ("!", &["TypeNever"]),
            ("(u8)", &["TypePath(u8)"]),
            (
                "Option<Result<u8, E>>",
                &["TypePath(Option)", "TypePath(Result)", "TypePath(E)"],
            ),
            (
                "ArrayVec<u8, 4>",
                &["TypePath(ArrayVec)", "TypePath(u8)", "LitInt"],
            ),
            ("Ref<'a, T>", &["TypePath(Ref)", "Lifetime", "TypePath(T)"]),
            ("*const u8", &["TypePtr", "TypePath(u8)"]),
            ("&mut u8", &["TypeRefMut", "TypePath(u8)"]),
            ("[u8]", &["TypeSlice", "TypePath(u8)"]),
            (
                "dyn Read + 'static",
                &["TypeTraitObject", "TraitBound(Read)", "LifetimeBound"],
            ),
            (
                "(u8, bool)",
                &["TypeTuple", "TypePath(u8)", "TypePath(bool)"],
            ),
        ] {
            assert_type_has_labels(src, labels);
        }

        let bound: syn::TypeParamBound = syn::parse_str("Fn(u8) -> bool").unwrap();
        let bound_tree = type_param_bound_tree(&bound);
        for label in ["TraitBound(Fn)", "TypePath(u8)", "ReturnType(bool)"] {
            assert!(
                has_label(&bound_tree, label),
                "missing parenthesized bound label {label}"
            );
        }

        let grouped = syn::Type::Group(syn::TypeGroup {
            group_token: Default::default(),
            elem: Box::new(syn::parse_str("u8").unwrap()),
        });
        let tree = type_tree(&grouped);
        assert!(
            has_label(&tree, "TypePath(u8)"),
            "missing grouped type child: {tree:?}"
        );
        assert_eq!(type_summary(&grouped), "u8");

        for (src, summary) in [
            ("[u8; 4]", "[u8]"),
            ("fn(u8) -> bool", "fn"),
            ("impl Iterator", "impl Trait"),
            ("_", "_"),
            ("ty_macro!()", "ty_macro"),
            ("!", "!"),
            ("(u8)", "u8"),
            ("*const u8", "*u8"),
            ("[u8]", "[u8]"),
            ("dyn Read", "dyn Trait"),
            ("(u8, bool)", "(u8,bool)"),
        ] {
            let ty: syn::Type = syn::parse_str(src).unwrap();
            assert_eq!(type_summary(&ty), summary);
        }
    }

    #[test]
    fn pattern_and_literal_projection_cover_rust_pattern_categories() {
        let cases: &[(&str, &[&str])] = &[
            ("mut x", &["PatIdentMut"]),
            ("1", &["LitInt"]),
            ("pat_macro!()", &["PatMacro(pat_macro)"]),
            (
                "Some(x) | None",
                &["PatOr", "PatTupleStruct(Some)", "PatIdent"],
            ),
            ("(x)", &["PatIdent"]),
            ("Option::None", &["PatPath(Option::None)"]),
            ("1..=3", &["PatRange", "LitInt"]),
            ("&x", &["PatReference", "PatIdent"]),
            ("..", &["PatRest"]),
            ("[head, ..]", &["PatSlice", "PatIdent", "PatRest"]),
            ("User { id }", &["PatStruct(User)", "PatField(id)"]),
            ("(a, b)", &["PatTuple", "PatIdent"]),
            ("Some(x)", &["PatTupleStruct(Some)", "PatIdent"]),
            ("_", &["PatWild"]),
        ];
        for (src, labels) in cases {
            assert_pat_has_labels(src, labels);
        }

        let pat_const = syn::Pat::Const(syn::parse_str("const { 1 }").unwrap());
        assert!(has_label(&pat_tree(&pat_const), "PatConst"));

        let pat_type = syn::Pat::Type(syn::PatType {
            attrs: Vec::new(),
            pat: Box::new(syn::Pat::parse_single.parse_str("x").unwrap()),
            colon_token: Default::default(),
            ty: Box::new(syn::parse_str("u8").unwrap()),
        });
        let pat_type_tree = pat_tree(&pat_type);
        assert!(has_label(&pat_type_tree, "PatType(u8)"));
        assert!(has_label(&pat_type_tree, "TypePath(u8)"));

        let verbatim = syn::Pat::Verbatim(quote::quote!(box value));
        assert!(has_label(&pat_tree(&verbatim), "PatVerbatim"));

        for (src, label) in [
            ("\"s\"", "LitStr"),
            ("b\"s\"", "LitByteStr"),
            ("b'x'", "LitByte"),
            ("'x'", "LitChar"),
            ("1", "LitInt"),
            ("1.0", "LitFloat"),
            ("true", "LitBool"),
        ] {
            let lit: syn::Lit = syn::parse_str(src).unwrap();
            assert_eq!(lit_label(&lit), label);
        }
        let verbatim_lit = syn::Lit::Verbatim(proc_macro2::Literal::usize_unsuffixed(1));
        assert_eq!(lit_label(&verbatim_lit), "Lit");
        assert_eq!(normalized_tokens(&quote::quote!(a + b)), "a + b");
    }

    #[test]
    fn same_control_flow_with_incompatible_signatures_scores_below_renamed_clone() {
        let compatible_src = r#"
struct UserId(u64);
struct User;
fn fallback_user() -> User { User }
fn load_user(id: UserId) -> User { User }

fn by_user_id(id: UserId) -> User {
    if id.0 == 0 {
        fallback_user()
    } else {
        load_user(id)
    }
}

fn by_other_user_id(other: UserId) -> User {
    if other.0 == 0 {
        fallback_user()
    } else {
        load_user(other)
    }
}
"#;
        let incompatible_src = r#"
struct UserId(u64);
struct OrderId(u64);
struct User;
struct Order;
fn fallback_user() -> User { User }
fn fallback_order() -> Order { Order }
fn load_user(id: UserId) -> User { User }
fn load_order(id: OrderId) -> Order { Order }

fn by_user_id(id: UserId) -> User {
    if id.0 == 0 {
        fallback_user()
    } else {
        load_user(id)
    }
}

fn by_order_id(id: OrderId) -> Order {
    if id.0 == 0 {
        fallback_order()
    } else {
        load_order(id)
    }
}
"#;
        let compatible = parse_functions(compatible_src);
        let incompatible = parse_functions(incompatible_src);
        let opts = TSEDOptions::default();
        let compatible_sim = calculate_tsed(&compatible[2].tree, &compatible[3].tree, &opts);
        let incompatible_sim = calculate_tsed(&incompatible[4].tree, &incompatible[5].tree, &opts);
        assert!(
            compatible_sim > 0.9,
            "expected renamed clone to stay high, got {compatible_sim}",
        );
        assert!(
            incompatible_sim < compatible_sim - 0.05,
            "expected incompatible signatures/calls to score lower, compatible={compatible_sim}, incompatible={incompatible_sim}",
        );
    }

    /// Default `extract_functions` keeps every item — even the ones an
    /// `--exclude-tests` run would drop. Walking into a `#[cfg(test)]
    /// mod` / `impl` / `trait` and the test-tagged free fn inside each
    /// must still surface them; otherwise the boolean guards in
    /// `collect_*` could degrade to constants (mutant `&& → ||`) and
    /// silently break the default contract without any test catching it.
    #[rstest]
    #[case::cfg_test_mod(
        "#[cfg(test)]\nmod tests { fn helper() {} }\n",
        &["helper"][..],
    )]
    #[case::cfg_test_impl(
        "struct T;\n#[cfg(test)]\nimpl T { fn helper() {} }\n",
        &["T::helper"][..],
    )]
    #[case::cfg_test_trait(
        "#[cfg(test)]\ntrait Tr { fn def_method() -> u32 { 0 } }\n",
        &["Tr::def_method"][..],
    )]
    #[case::test_attr_free_fn("#[test]\nfn ut() {}\n", &["ut"][..])]
    #[case::test_attr_impl_method(
        "struct T;\nimpl T { #[test] fn fixture() {} }\n",
        &["T::fixture"][..],
    )]
    #[case::test_attr_trait_method(
        "trait Tr { #[test] fn default_test() -> u32 { 0 } }\n",
        &["Tr::default_test"][..],
    )]
    fn default_extraction_includes_test_attributed_items(
        #[case] src: &str,
        #[case] expected: &[&str],
    ) {
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names, expected,
            "default extraction must keep every item; only --exclude-tests should drop them",
        );
    }

    #[test]
    fn extraction_marks_cfg_test_modules_and_test_attributed_fns() {
        // Production code surrounded by every shape the analyzer later
        // filters: a `#[test]` free fn, a `#[rstest]` fn, a `mod tests`
        // gated by `#[cfg(test)]`, and an `impl` block gated the same way.
        let src = r#"
fn production(x: i32) -> i32 { x + 1 }

#[test]
fn unit_test() { assert_eq!(production(0), 1); }

#[rstest]
fn parameterised_test() { assert_eq!(production(0), 1); }

#[cfg(test)]
mod tests {
    use super::*;
    fn helper() -> i32 { production(0) }
    fn other_helper() -> i32 { production(1) }
}

struct Bag;
#[cfg(test)]
impl Bag {
    fn fixture() -> Self { Self }
}
"#;
        let mut parser = RustParser::new();
        let funcs = parser.extract_functions(src).unwrap();
        let flags: Vec<_> = funcs.iter().map(|f| (f.name.as_str(), f.is_test)).collect();
        assert_eq!(
            flags,
            [
                ("production", false),
                ("unit_test", true),
                ("parameterised_test", true),
                ("helper", true),
                ("other_helper", true),
                ("Bag::fixture", true),
            ]
        );
    }

    #[test]
    fn extraction_marks_functions_without_test_attrs_as_production() {
        let src = "fn a() {}\nfn b() {}\n";
        let funcs = parse_functions(src);
        assert!(funcs.iter().all(|f| !f.is_test));
    }

    #[test]
    fn find_similar_functions_reports_clone_pair() {
        let src = r#"
fn a(xs: &[u32]) -> u32 {
    let mut t = 0;
    for x in xs { t += *x; }
    t
}

fn b(ys: &[u32]) -> u32 {
    let mut s = 0;
    for y in ys { s += *y; }
    s
}

fn c(n: u32) -> u32 {
    if n == 0 { 0 } else { n * 2 }
}
"#;
        let funcs = parse_functions(src);
        let pairs = find_similar_functions(&funcs, 0.85, &TSEDOptions::default());
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a.name, "a");
        assert_eq!(pairs[0].b.name, "b");
    }
}

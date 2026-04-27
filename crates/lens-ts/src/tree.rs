//! Lower an oxc AST node into a generic [`lens_domain::TreeNode`].
//!
//! Each AST node becomes a [`TreeNode`] whose `label` is its node kind
//! (e.g. `IfStatement`, `BinaryExpression`, `Identifier`). Identifiers
//! and literals carry their textual value as `value` so APTED's optional
//! value-level matching can distinguish them.
//!
//! Coverage is intentionally pragmatic: control flow, expressions,
//! literals, and JSX are mapped explicitly; less-frequent forms
//! (decorators, TypeScript-only type-level constructs) collapse to a
//! generic label so they still take up structural space without each
//! node kind demanding its own arm. APTED only needs *consistent*
//! labels to score similarity, not exhaustive coverage.
//!
//! JSX subtrees retain their element name, attribute names, and child
//! arity so two React components with different markup do not collapse
//! to the same shape — without that, every `function () { return <X />; }`
//! body normalises to the same single-leaf return and scores 1.0 against
//! every other component.

use lens_domain::TreeNode;
use oxc_ast::ast::*;

pub fn function_body_tree(body: &FunctionBody) -> TreeNode {
    node_with("FunctionBody", body.statements.iter().map(stmt_tree))
}

/// Build a label-only node and attach the given children. Centralising the
/// boilerplate keeps each match arm to a single expression.
fn node_with(label: &str, children: impl IntoIterator<Item = TreeNode>) -> TreeNode {
    let mut n = TreeNode::new(label, "");
    for c in children {
        n.push_child(c);
    }
    n
}

fn stmt_tree(stmt: &Statement) -> TreeNode {
    match stmt {
        Statement::BlockStatement(b) => node_with("Block", b.body.iter().map(stmt_tree)),
        Statement::IfStatement(it) => if_stmt(it),
        Statement::WhileStatement(w) => {
            node_with("While", [expr_tree(&w.test), stmt_tree(&w.body)])
        }
        Statement::DoWhileStatement(w) => {
            node_with("DoWhile", [stmt_tree(&w.body), expr_tree(&w.test)])
        }
        Statement::ForStatement(f) => for_stmt(f),
        Statement::ForInStatement(f) => node_with("ForIn", [stmt_tree(&f.body)]),
        Statement::ForOfStatement(f) => {
            node_with("ForOf", [expr_tree(&f.right), stmt_tree(&f.body)])
        }
        Statement::SwitchStatement(s) => switch_stmt(s),
        Statement::ReturnStatement(r) => node_with("Return", r.argument.as_ref().map(expr_tree)),
        Statement::ThrowStatement(t) => node_with("Throw", [expr_tree(&t.argument)]),
        Statement::TryStatement(t) => try_stmt(t),
        Statement::ExpressionStatement(e) => node_with("ExprStmt", [expr_tree(&e.expression)]),
        Statement::VariableDeclaration(v) => var_decl_node(v),
        Statement::BreakStatement(_) => TreeNode::leaf("Break"),
        Statement::ContinueStatement(_) => TreeNode::leaf("Continue"),
        Statement::EmptyStatement(_) => TreeNode::leaf("Empty"),
        Statement::LabeledStatement(l) => node_with("Labeled", [stmt_tree(&l.body)]),
        Statement::FunctionDeclaration(_) => TreeNode::leaf("FunctionDecl"),
        Statement::ClassDeclaration(_) => TreeNode::leaf("ClassDecl"),
        _ => TreeNode::leaf("Stmt"),
    }
}

fn if_stmt(it: &IfStatement) -> TreeNode {
    let mut children = vec![expr_tree(&it.test), stmt_tree(&it.consequent)];
    if let Some(alt) = &it.alternate {
        children.push(stmt_tree(alt));
    }
    node_with("If", children)
}

fn for_stmt(f: &ForStatement) -> TreeNode {
    let mut children = Vec::new();
    if let Some(init) = &f.init {
        children.push(for_init_tree(init));
    }
    if let Some(test) = &f.test {
        children.push(expr_tree(test));
    }
    if let Some(update) = &f.update {
        children.push(expr_tree(update));
    }
    children.push(stmt_tree(&f.body));
    node_with("For", children)
}

fn switch_stmt(s: &SwitchStatement) -> TreeNode {
    let mut children = vec![expr_tree(&s.discriminant)];
    children.extend(s.cases.iter().map(|case| {
        let test = case.test.as_ref().map(expr_tree);
        let body = case.consequent.iter().map(stmt_tree);
        node_with("Case", test.into_iter().chain(body))
    }));
    node_with("Switch", children)
}

fn try_stmt(t: &TryStatement) -> TreeNode {
    let block = node_with("Block", t.block.body.iter().map(stmt_tree));
    let handler = t
        .handler
        .as_ref()
        .map(|h| node_with("Catch", h.body.body.iter().map(stmt_tree)));
    let finalizer = t
        .finalizer
        .as_ref()
        .map(|f| node_with("Finally", f.body.iter().map(stmt_tree)));
    node_with(
        "Try",
        std::iter::once(block).chain(handler).chain(finalizer),
    )
}

fn var_decl_node(v: &VariableDeclaration) -> TreeNode {
    let mut n = TreeNode::new("VarDecl", v.kind.as_str());
    for d in &v.declarations {
        n.push_child(node_with("Declarator", d.init.as_ref().map(expr_tree)));
    }
    n
}

fn for_init_tree(init: &ForStatementInit) -> TreeNode {
    match init {
        ForStatementInit::VariableDeclaration(v) => var_decl_node(v),
        // The remaining variants are expression-shaped.
        _ => TreeNode::leaf("ForInit"),
    }
}

pub fn expr_tree(expr: &Expression) -> TreeNode {
    match expr {
        // Identifiers and literals — values matter for APTED's value-level
        // matching, so they are passed through verbatim.
        Expression::Identifier(id) => TreeNode::new("Ident", id.name.as_str()),
        Expression::StringLiteral(s) => TreeNode::new("Lit", s.value.as_str()),
        Expression::NumericLiteral(n) => TreeNode::new("Lit", n.raw_str()),
        Expression::BigIntLiteral(b) => TreeNode::new("Lit", b.raw.as_deref().unwrap_or("")),
        Expression::BooleanLiteral(b) => {
            TreeNode::new("Lit", if b.value { "true" } else { "false" })
        }
        Expression::NullLiteral(_) => TreeNode::new("Lit", "null"),
        Expression::TemplateLiteral(_) => TreeNode::leaf("Template"),
        Expression::RegExpLiteral(_) => TreeNode::leaf("Regex"),

        // Unary / binary / logical operators carry the operator as value.
        Expression::BinaryExpression(b) => {
            let mut n = TreeNode::new("Binary", b.operator.as_str());
            n.push_child(expr_tree(&b.left));
            n.push_child(expr_tree(&b.right));
            n
        }
        Expression::LogicalExpression(l) => {
            let mut n = TreeNode::new("Logical", l.operator.as_str());
            n.push_child(expr_tree(&l.left));
            n.push_child(expr_tree(&l.right));
            n
        }
        Expression::UnaryExpression(u) => {
            let mut n = TreeNode::new("Unary", u.operator.as_str());
            n.push_child(expr_tree(&u.argument));
            n
        }
        Expression::UpdateExpression(u) => TreeNode::new("Update", u.operator.as_str()),
        Expression::AssignmentExpression(a) => {
            let mut n = TreeNode::new("Assign", a.operator.as_str());
            n.push_child(expr_tree(&a.right));
            n
        }
        Expression::ConditionalExpression(c) => node_with(
            "Conditional",
            [
                expr_tree(&c.test),
                expr_tree(&c.consequent),
                expr_tree(&c.alternate),
            ],
        ),

        // Calls and member access.
        Expression::CallExpression(c) => call_like("Call", &c.callee, &c.arguments),
        Expression::NewExpression(c) => call_like("New", &c.callee, &c.arguments),
        Expression::StaticMemberExpression(m) => {
            let mut n = TreeNode::new("Member", m.property.name.as_str());
            n.push_child(expr_tree(&m.object));
            n
        }
        Expression::ComputedMemberExpression(m) => node_with(
            "ComputedMember",
            [expr_tree(&m.object), expr_tree(&m.expression)],
        ),

        // Aggregates.
        Expression::ArrayExpression(a) => array_expr(a),
        Expression::ObjectExpression(o) => object_expr(o),

        // Functions / async.
        Expression::ArrowFunctionExpression(_) => TreeNode::leaf("Arrow"),
        Expression::FunctionExpression(_) => TreeNode::leaf("FunctionExpr"),
        Expression::AwaitExpression(a) => node_with("Await", [expr_tree(&a.argument)]),
        Expression::YieldExpression(y) => node_with("Yield", y.argument.as_ref().map(expr_tree)),
        Expression::SequenceExpression(s) => {
            node_with("Sequence", s.expressions.iter().map(expr_tree))
        }

        // Pass-through forms: the wrapper itself adds no useful structure
        // for similarity, so unwrap to the inner expression.
        Expression::ParenthesizedExpression(p) => expr_tree(&p.expression),
        Expression::TSAsExpression(a) => expr_tree(&a.expression),
        Expression::TSSatisfiesExpression(s) => expr_tree(&s.expression),
        Expression::TSNonNullExpression(n) => expr_tree(&n.expression),
        Expression::TSTypeAssertion(t) => expr_tree(&t.expression),

        // Reserved keywords that are leaves.
        Expression::ThisExpression(_) => TreeNode::leaf("This"),
        Expression::Super(_) => TreeNode::leaf("Super"),

        // JSX. Without explicit handling these collapse to a single
        // `Expr` leaf, which makes every React component look identical
        // to every other one (issue #65).
        Expression::JSXElement(e) => jsx_element_tree(e),
        Expression::JSXFragment(f) => jsx_fragment_tree(f),

        _ => TreeNode::leaf("Expr"),
    }
}

fn jsx_element_tree(el: &JSXElement) -> TreeNode {
    let name = jsx_element_name(&el.opening_element.name);
    let mut n = TreeNode::new("JSXElement", name);
    for attr in &el.opening_element.attributes {
        n.push_child(jsx_attribute_item_tree(attr));
    }
    for child in &el.children {
        n.push_child(jsx_child_tree(child));
    }
    n
}

fn jsx_fragment_tree(f: &JSXFragment) -> TreeNode {
    node_with("JSXFragment", f.children.iter().map(jsx_child_tree))
}

fn jsx_element_name(name: &JSXElementName) -> String {
    match name {
        JSXElementName::Identifier(id) => id.name.to_string(),
        JSXElementName::IdentifierReference(id) => id.name.to_string(),
        JSXElementName::NamespacedName(n) => {
            format!("{}:{}", n.namespace.name, n.name.name)
        }
        JSXElementName::MemberExpression(m) => jsx_member_expression_name(m),
        JSXElementName::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_member_expression_name(m: &JSXMemberExpression) -> String {
    let object = match &m.object {
        JSXMemberExpressionObject::IdentifierReference(id) => id.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(inner) => jsx_member_expression_name(inner),
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    };
    format!("{}.{}", object, m.property.name)
}

fn jsx_attribute_item_tree(item: &JSXAttributeItem) -> TreeNode {
    match item {
        JSXAttributeItem::Attribute(a) => jsx_attribute_tree(a),
        JSXAttributeItem::SpreadAttribute(s) => {
            node_with("JSXSpreadAttr", [expr_tree(&s.argument)])
        }
    }
}

fn jsx_attribute_tree(a: &JSXAttribute) -> TreeNode {
    let name = match &a.name {
        JSXAttributeName::Identifier(id) => id.name.to_string(),
        JSXAttributeName::NamespacedName(n) => {
            format!("{}:{}", n.namespace.name, n.name.name)
        }
    };
    let mut n = TreeNode::new("JSXAttr", name);
    if let Some(value) = &a.value {
        n.push_child(jsx_attribute_value_tree(value));
    }
    n
}

fn jsx_attribute_value_tree(value: &JSXAttributeValue) -> TreeNode {
    match value {
        JSXAttributeValue::StringLiteral(s) => TreeNode::new("Lit", s.value.as_str()),
        JSXAttributeValue::ExpressionContainer(c) => jsx_expression_tree(&c.expression),
        JSXAttributeValue::Element(e) => jsx_element_tree(e),
        JSXAttributeValue::Fragment(f) => jsx_fragment_tree(f),
    }
}

fn jsx_child_tree(child: &JSXChild) -> TreeNode {
    match child {
        JSXChild::Text(_) => TreeNode::leaf("JSXText"),
        JSXChild::Element(e) => jsx_element_tree(e),
        JSXChild::Fragment(f) => jsx_fragment_tree(f),
        JSXChild::ExpressionContainer(c) => {
            node_with("JSXExprContainer", [jsx_expression_tree(&c.expression)])
        }
        JSXChild::Spread(s) => node_with("JSXSpreadChild", [expr_tree(&s.expression)]),
    }
}

fn jsx_expression_tree(expr: &JSXExpression) -> TreeNode {
    match expr {
        JSXExpression::EmptyExpression(_) => TreeNode::leaf("JSXEmpty"),
        // `JSXExpression` inherits all `Expression` variants via `inherit_variants!`,
        // so the conversion routes them through the standard expression lowering.
        other => match other.as_expression() {
            Some(e) => expr_tree(e),
            None => TreeNode::leaf("JSXEmpty"),
        },
    }
}

fn call_like(label: &str, callee: &Expression, args: &[Argument]) -> TreeNode {
    let children = std::iter::once(expr_tree(callee)).chain(args.iter().map(argument_tree));
    node_with(label, children)
}

fn array_expr(a: &ArrayExpression) -> TreeNode {
    node_with(
        "Array",
        a.elements.iter().map(|el| match el {
            ArrayExpressionElement::SpreadElement(s) => {
                node_with("Spread", [expr_tree(&s.argument)])
            }
            other => match other.as_expression() {
                Some(expr) => expr_tree(expr),
                None => TreeNode::leaf("Hole"),
            },
        }),
    )
}

fn object_expr(o: &ObjectExpression) -> TreeNode {
    node_with(
        "Object",
        o.properties.iter().map(|prop| match prop {
            ObjectPropertyKind::ObjectProperty(_) => TreeNode::leaf("Property"),
            ObjectPropertyKind::SpreadProperty(s) => node_with("Spread", [expr_tree(&s.argument)]),
        }),
    )
}

fn argument_tree(arg: &Argument) -> TreeNode {
    if let Argument::SpreadElement(s) = arg {
        return node_with("Spread", [expr_tree(&s.argument)]);
    }
    match arg.as_expression() {
        Some(expr) => expr_tree(expr),
        None => TreeNode::leaf("Arg"),
    }
}

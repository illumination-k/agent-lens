//! Lower an oxc AST node into a generic [`lens_domain::TreeNode`].
//!
//! Each AST node becomes a [`TreeNode`] whose `label` is its node kind
//! (e.g. `IfStatement`, `BinaryExpression`, `Identifier`). Identifiers
//! and literals carry their textual value as `value` so APTED's optional
//! value-level matching can distinguish them.
//!
//! Coverage is intentionally pragmatic: control flow, expressions, and
//! literals are mapped explicitly; less-frequent forms (decorators,
//! TypeScript-only type-level constructs, JSX) collapse to a generic
//! label so they still take up structural space without each node kind
//! demanding its own arm. APTED only needs *consistent* labels to score
//! similarity, not exhaustive coverage.

use lens_domain::TreeNode;
use oxc_ast::ast::*;

pub fn function_body_tree(body: &FunctionBody) -> TreeNode {
    let mut node = TreeNode::new("FunctionBody", "");
    for stmt in &body.statements {
        node.push_child(stmt_tree(stmt));
    }
    node
}

pub fn expression_tree(expr: &Expression) -> TreeNode {
    expr_tree(expr)
}

fn stmt_tree(stmt: &Statement) -> TreeNode {
    match stmt {
        Statement::BlockStatement(b) => {
            let mut n = TreeNode::new("Block", "");
            for s in &b.body {
                n.push_child(stmt_tree(s));
            }
            n
        }
        Statement::IfStatement(it) => {
            let mut n = TreeNode::new("If", "");
            n.push_child(expr_tree(&it.test));
            n.push_child(stmt_tree(&it.consequent));
            if let Some(alt) = &it.alternate {
                n.push_child(stmt_tree(alt));
            }
            n
        }
        Statement::WhileStatement(w) => {
            let mut n = TreeNode::new("While", "");
            n.push_child(expr_tree(&w.test));
            n.push_child(stmt_tree(&w.body));
            n
        }
        Statement::DoWhileStatement(w) => {
            let mut n = TreeNode::new("DoWhile", "");
            n.push_child(stmt_tree(&w.body));
            n.push_child(expr_tree(&w.test));
            n
        }
        Statement::ForStatement(f) => {
            let mut n = TreeNode::new("For", "");
            if let Some(init) = &f.init {
                n.push_child(for_init_tree(init));
            }
            if let Some(test) = &f.test {
                n.push_child(expr_tree(test));
            }
            if let Some(update) = &f.update {
                n.push_child(expr_tree(update));
            }
            n.push_child(stmt_tree(&f.body));
            n
        }
        Statement::ForInStatement(f) => {
            let mut n = TreeNode::new("ForIn", "");
            n.push_child(stmt_tree(&f.body));
            n
        }
        Statement::ForOfStatement(f) => {
            let mut n = TreeNode::new("ForOf", "");
            n.push_child(expr_tree(&f.right));
            n.push_child(stmt_tree(&f.body));
            n
        }
        Statement::SwitchStatement(s) => {
            let mut n = TreeNode::new("Switch", "");
            n.push_child(expr_tree(&s.discriminant));
            for case in &s.cases {
                let mut c = TreeNode::new("Case", "");
                if let Some(t) = &case.test {
                    c.push_child(expr_tree(t));
                }
                for stmt in &case.consequent {
                    c.push_child(stmt_tree(stmt));
                }
                n.push_child(c);
            }
            n
        }
        Statement::ReturnStatement(r) => {
            let mut n = TreeNode::new("Return", "");
            if let Some(arg) = &r.argument {
                n.push_child(expr_tree(arg));
            }
            n
        }
        Statement::ThrowStatement(t) => {
            let mut n = TreeNode::new("Throw", "");
            n.push_child(expr_tree(&t.argument));
            n
        }
        Statement::TryStatement(t) => {
            let mut n = TreeNode::new("Try", "");
            let mut block = TreeNode::new("Block", "");
            for s in &t.block.body {
                block.push_child(stmt_tree(s));
            }
            n.push_child(block);
            if let Some(handler) = &t.handler {
                let mut h = TreeNode::new("Catch", "");
                for s in &handler.body.body {
                    h.push_child(stmt_tree(s));
                }
                n.push_child(h);
            }
            if let Some(finalizer) = &t.finalizer {
                let mut f = TreeNode::new("Finally", "");
                for s in &finalizer.body {
                    f.push_child(stmt_tree(s));
                }
                n.push_child(f);
            }
            n
        }
        Statement::ExpressionStatement(e) => {
            let mut n = TreeNode::new("ExprStmt", "");
            n.push_child(expr_tree(&e.expression));
            n
        }
        Statement::VariableDeclaration(v) => {
            let mut n = TreeNode::new("VarDecl", v.kind.as_str());
            for d in &v.declarations {
                let mut decl = TreeNode::new("Declarator", "");
                if let Some(init) = &d.init {
                    decl.push_child(expr_tree(init));
                }
                n.push_child(decl);
            }
            n
        }
        Statement::BreakStatement(_) => TreeNode::leaf("Break"),
        Statement::ContinueStatement(_) => TreeNode::leaf("Continue"),
        Statement::EmptyStatement(_) => TreeNode::leaf("Empty"),
        Statement::LabeledStatement(l) => {
            let mut n = TreeNode::new("Labeled", "");
            n.push_child(stmt_tree(&l.body));
            n
        }
        Statement::FunctionDeclaration(_) => TreeNode::leaf("FunctionDecl"),
        Statement::ClassDeclaration(_) => TreeNode::leaf("ClassDecl"),
        _ => TreeNode::leaf("Stmt"),
    }
}

fn for_init_tree(init: &ForStatementInit) -> TreeNode {
    match init {
        ForStatementInit::VariableDeclaration(v) => {
            let mut n = TreeNode::new("VarDecl", v.kind.as_str());
            for d in &v.declarations {
                let mut decl = TreeNode::new("Declarator", "");
                if let Some(i) = &d.init {
                    decl.push_child(expr_tree(i));
                }
                n.push_child(decl);
            }
            n
        }
        // The remaining variants are expression-shaped.
        _ => TreeNode::leaf("ForInit"),
    }
}

fn expr_tree(expr: &Expression) -> TreeNode {
    match expr {
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
        Expression::ConditionalExpression(c) => {
            let mut n = TreeNode::new("Conditional", "");
            n.push_child(expr_tree(&c.test));
            n.push_child(expr_tree(&c.consequent));
            n.push_child(expr_tree(&c.alternate));
            n
        }
        Expression::CallExpression(c) => {
            let mut n = TreeNode::new("Call", "");
            n.push_child(expr_tree(&c.callee));
            for arg in &c.arguments {
                n.push_child(argument_tree(arg));
            }
            n
        }
        Expression::NewExpression(c) => {
            let mut n = TreeNode::new("New", "");
            n.push_child(expr_tree(&c.callee));
            for arg in &c.arguments {
                n.push_child(argument_tree(arg));
            }
            n
        }
        Expression::StaticMemberExpression(m) => {
            let mut n = TreeNode::new("Member", m.property.name.as_str());
            n.push_child(expr_tree(&m.object));
            n
        }
        Expression::ComputedMemberExpression(m) => {
            let mut n = TreeNode::new("ComputedMember", "");
            n.push_child(expr_tree(&m.object));
            n.push_child(expr_tree(&m.expression));
            n
        }
        Expression::ArrayExpression(a) => {
            let mut n = TreeNode::new("Array", "");
            for el in &a.elements {
                if let ArrayExpressionElement::SpreadElement(s) = el {
                    let mut sp = TreeNode::new("Spread", "");
                    sp.push_child(expr_tree(&s.argument));
                    n.push_child(sp);
                } else if let Some(expr) = el.as_expression() {
                    n.push_child(expr_tree(expr));
                } else {
                    n.push_child(TreeNode::leaf("Hole"));
                }
            }
            n
        }
        Expression::ObjectExpression(o) => {
            let mut n = TreeNode::new("Object", "");
            for prop in &o.properties {
                n.push_child(match prop {
                    ObjectPropertyKind::ObjectProperty(_) => TreeNode::leaf("Property"),
                    ObjectPropertyKind::SpreadProperty(s) => {
                        let mut sp = TreeNode::new("Spread", "");
                        sp.push_child(expr_tree(&s.argument));
                        sp
                    }
                });
            }
            n
        }
        Expression::ArrowFunctionExpression(_) => TreeNode::leaf("Arrow"),
        Expression::FunctionExpression(_) => TreeNode::leaf("FunctionExpr"),
        Expression::AwaitExpression(a) => {
            let mut n = TreeNode::new("Await", "");
            n.push_child(expr_tree(&a.argument));
            n
        }
        Expression::YieldExpression(y) => {
            let mut n = TreeNode::new("Yield", "");
            if let Some(arg) = &y.argument {
                n.push_child(expr_tree(arg));
            }
            n
        }
        Expression::SequenceExpression(s) => {
            let mut n = TreeNode::new("Sequence", "");
            for e in &s.expressions {
                n.push_child(expr_tree(e));
            }
            n
        }
        Expression::ParenthesizedExpression(p) => expr_tree(&p.expression),
        Expression::ThisExpression(_) => TreeNode::leaf("This"),
        Expression::Super(_) => TreeNode::leaf("Super"),
        Expression::TSAsExpression(a) => expr_tree(&a.expression),
        Expression::TSSatisfiesExpression(s) => expr_tree(&s.expression),
        Expression::TSNonNullExpression(n) => expr_tree(&n.expression),
        Expression::TSTypeAssertion(t) => expr_tree(&t.expression),
        _ => TreeNode::leaf("Expr"),
    }
}

fn argument_tree(arg: &Argument) -> TreeNode {
    if let Argument::SpreadElement(s) = arg {
        let mut n = TreeNode::new("Spread", "");
        n.push_child(expr_tree(&s.argument));
        return n;
    }
    if let Some(expr) = arg.as_expression() {
        expr_tree(expr)
    } else {
        TreeNode::leaf("Arg")
    }
}

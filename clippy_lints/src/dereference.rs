use clippy_utils::diagnostics::{span_lint_and_sugg, span_lint_hir_and_then};
use clippy_utils::source::{snippet_with_applicability, snippet_with_context};
use clippy_utils::sugg::has_enclosing_paren;
use clippy_utils::ty::{expr_sig, peel_mid_ty_refs, variant_of_res};
use clippy_utils::{
    get_parent_expr, get_parent_node, is_lint_allowed, path_to_local, peel_hir_ty_refs, walk_to_expr_usage,
};
use rustc_ast::util::parser::{PREC_POSTFIX, PREC_PREFIX};
use rustc_data_structures::fx::FxIndexMap;
use rustc_errors::Applicability;
use rustc_hir::{
    self as hir, BindingAnnotation, Body, BodyId, BorrowKind, Destination, Expr, ExprKind, FnRetTy, GenericArg, HirId,
    ImplItem, ImplItemKind, Item, ItemKind, Local, MatchSource, Mutability, Node, Pat, PatKind, Path, QPath, TraitItem,
    TraitItemKind, TyKind, UnOp,
};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::adjustment::{Adjust, Adjustment, AutoBorrow, AutoBorrowMutability};
use rustc_middle::ty::{self, Ty, TyCtxt, TypeFoldable, TypeckResults};
use rustc_session::{declare_tool_lint, impl_lint_pass};
use rustc_span::{symbol::sym, Span};

declare_clippy_lint! {
    /// ### What it does
    /// Checks for explicit `deref()` or `deref_mut()` method calls.
    ///
    /// ### Why is this bad?
    /// Dereferencing by `&*x` or `&mut *x` is clearer and more concise,
    /// when not part of a method chain.
    ///
    /// ### Example
    /// ```rust
    /// use std::ops::Deref;
    /// let a: &mut String = &mut String::from("foo");
    /// let b: &str = a.deref();
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// let a: &mut String = &mut String::from("foo");
    /// let b = &*a;
    /// ```
    ///
    /// This lint excludes:
    /// ```rust,ignore
    /// let _ = d.unwrap().deref();
    /// ```
    #[clippy::version = "1.44.0"]
    pub EXPLICIT_DEREF_METHODS,
    pedantic,
    "Explicit use of deref or deref_mut method while not in a method chain."
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for address of operations (`&`) that are going to
    /// be dereferenced immediately by the compiler.
    ///
    /// ### Why is this bad?
    /// Suggests that the receiver of the expression borrows
    /// the expression.
    ///
    /// ### Example
    /// ```rust
    /// fn fun(_a: &i32) {}
    ///
    /// let x: &i32 = &&&&&&5;
    /// fun(&x);
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// # fn fun(_a: &i32) {}
    /// let x: &i32 = &5;
    /// fun(x);
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub NEEDLESS_BORROW,
    style,
    "taking a reference that is going to be automatically dereferenced"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for `ref` bindings which create a reference to a reference.
    ///
    /// ### Why is this bad?
    /// The address-of operator at the use site is clearer about the need for a reference.
    ///
    /// ### Example
    /// ```rust
    /// let x = Some("");
    /// if let Some(ref x) = x {
    ///     // use `x` here
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// let x = Some("");
    /// if let Some(x) = x {
    ///     // use `&x` here
    /// }
    /// ```
    #[clippy::version = "1.54.0"]
    pub REF_BINDING_TO_REFERENCE,
    pedantic,
    "`ref` binding to a reference"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for dereferencing expressions which would be covered by auto-deref.
    ///
    /// ### Why is this bad?
    /// This unnecessarily complicates the code.
    ///
    /// ### Example
    /// ```rust
    /// let x = String::new();
    /// let y: &str = &*x;
    /// ```
    /// Use instead:
    /// ```rust
    /// let x = String::new();
    /// let y: &str = &x;
    /// ```
    #[clippy::version = "1.60.0"]
    pub EXPLICIT_AUTO_DEREF,
    complexity,
    "dereferencing when the compiler would automatically dereference"
}

impl_lint_pass!(Dereferencing => [
    EXPLICIT_DEREF_METHODS,
    NEEDLESS_BORROW,
    REF_BINDING_TO_REFERENCE,
    EXPLICIT_AUTO_DEREF,
]);

#[derive(Default)]
pub struct Dereferencing {
    state: Option<(State, StateData)>,

    // While parsing a `deref` method call in ufcs form, the path to the function is itself an
    // expression. This is to store the id of that expression so it can be skipped when
    // `check_expr` is called for it.
    skip_expr: Option<HirId>,

    /// The body the first local was found in. Used to emit lints when the traversal of the body has
    /// been finished. Note we can't lint at the end of every body as they can be nested within each
    /// other.
    current_body: Option<BodyId>,
    /// The list of locals currently being checked by the lint.
    /// If the value is `None`, then the binding has been seen as a ref pattern, but is not linted.
    /// This is needed for or patterns where one of the branches can be linted, but another can not
    /// be.
    ///
    /// e.g. `m!(x) | Foo::Bar(ref x)`
    ref_locals: FxIndexMap<HirId, Option<RefPat>>,
}

struct StateData {
    /// Span of the top level expression
    span: Span,
    hir_id: HirId,
}

enum State {
    // Any number of deref method calls.
    DerefMethod {
        // The number of calls in a sequence which changed the referenced type
        ty_changed_count: usize,
        is_final_ufcs: bool,
        /// The required mutability
        target_mut: Mutability,
    },
    DerefedBorrow {
        count: usize,
        required_precedence: i8,
        msg: &'static str,
    },
    ExplicitDeref {
        deref_span: Span,
        deref_hir_id: HirId,
    },
    Reborrow {
        deref_span: Span,
        deref_hir_id: HirId,
    },
    Borrow,
}

// A reference operation considered by this lint pass
enum RefOp {
    Method(Mutability),
    Deref,
    AddrOf,
}

struct RefPat {
    /// Whether every usage of the binding is dereferenced.
    always_deref: bool,
    /// The spans of all the ref bindings for this local.
    spans: Vec<Span>,
    /// The applicability of this suggestion.
    app: Applicability,
    /// All the replacements which need to be made.
    replacements: Vec<(Span, String)>,
    /// The [`HirId`] that the lint should be emitted at.
    hir_id: HirId,
}

impl<'tcx> LateLintPass<'tcx> for Dereferencing {
    #[expect(clippy::too_many_lines)]
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>) {
        // Skip path expressions from deref calls. e.g. `Deref::deref(e)`
        if Some(expr.hir_id) == self.skip_expr.take() {
            return;
        }

        if let Some(local) = path_to_local(expr) {
            self.check_local_usage(cx, expr, local);
        }

        // Stop processing sub expressions when a macro call is seen
        if expr.span.from_expansion() {
            if let Some((state, data)) = self.state.take() {
                report(cx, expr, state, data);
            }
            return;
        }

        let typeck = cx.typeck_results();
        let (kind, sub_expr) = if let Some(x) = try_parse_ref_op(cx.tcx, typeck, expr) {
            x
        } else {
            // The whole chain of reference operations has been seen
            if let Some((state, data)) = self.state.take() {
                report(cx, expr, state, data);
            }
            return;
        };

        match (self.state.take(), kind) {
            (None, kind) => {
                let parent = get_parent_node(cx.tcx, expr.hir_id);
                let expr_ty = typeck.expr_ty(expr);

                match kind {
                    RefOp::Method(target_mut)
                        if !is_lint_allowed(cx, EXPLICIT_DEREF_METHODS, expr.hir_id)
                            && is_linted_explicit_deref_position(parent, expr.hir_id, expr.span) =>
                    {
                        self.state = Some((
                            State::DerefMethod {
                                ty_changed_count: if deref_method_same_type(expr_ty, typeck.expr_ty(sub_expr)) {
                                    0
                                } else {
                                    1
                                },
                                is_final_ufcs: matches!(expr.kind, ExprKind::Call(..)),
                                target_mut,
                            },
                            StateData {
                                span: expr.span,
                                hir_id: expr.hir_id,
                            },
                        ));
                    },
                    RefOp::AddrOf => {
                        // Find the number of times the borrow is auto-derefed.
                        let mut iter = find_adjustments(cx.tcx, typeck, expr).iter();
                        let mut deref_count = 0usize;
                        let next_adjust = loop {
                            match iter.next() {
                                Some(adjust) => {
                                    if !matches!(adjust.kind, Adjust::Deref(_)) {
                                        break Some(adjust);
                                    } else if !adjust.target.is_ref() {
                                        deref_count += 1;
                                        break iter.next();
                                    }
                                    deref_count += 1;
                                },
                                None => break None,
                            };
                        };

                        // Determine the required number of references before any can be removed. In all cases the
                        // reference made by the current expression will be removed. After that there are four cases to
                        // handle.
                        //
                        // 1. Auto-borrow will trigger in the current position, so no further references are required.
                        // 2. Auto-deref ends at a reference, or the underlying type, so one extra needs to be left to
                        //    handle the automatically inserted re-borrow.
                        // 3. Auto-deref hits a user-defined `Deref` impl, so at least one reference needs to exist to
                        //    start auto-deref.
                        // 4. If the chain of non-user-defined derefs ends with a mutable re-borrow, and re-borrow
                        //    adjustments will not be inserted automatically, then leave one further reference to avoid
                        //    moving a mutable borrow.
                        //    e.g.
                        //        fn foo<T>(x: &mut Option<&mut T>, y: &mut T) {
                        //            let x = match x {
                        //                // Removing the borrow will cause `x` to be moved
                        //                Some(x) => &mut *x,
                        //                None => y
                        //            };
                        //        }
                        let deref_msg =
                            "this expression creates a reference which is immediately dereferenced by the compiler";
                        let borrow_msg = "this expression borrows a value the compiler would automatically borrow";

                        let (required_refs, required_precedence, msg) = if is_auto_borrow_position(parent, expr.hir_id)
                        {
                            (1, PREC_POSTFIX, if deref_count == 1 { borrow_msg } else { deref_msg })
                        } else if let Some(&Adjust::Borrow(AutoBorrow::Ref(_, mutability))) =
                            next_adjust.map(|a| &a.kind)
                        {
                            if matches!(mutability, AutoBorrowMutability::Mut { .. })
                                && !is_auto_reborrow_position(parent)
                            {
                                (3, 0, deref_msg)
                            } else {
                                (2, 0, deref_msg)
                            }
                        } else {
                            (2, 0, deref_msg)
                        };

                        if deref_count >= required_refs {
                            self.state = Some((
                                State::DerefedBorrow {
                                    // One of the required refs is for the current borrow expression, the remaining ones
                                    // can't be removed without breaking the code. See earlier comment.
                                    count: deref_count - required_refs,
                                    required_precedence,
                                    msg,
                                },
                                StateData {
                                    span: expr.span,
                                    hir_id: expr.hir_id,
                                },
                            ));
                        } else if is_stable_auto_deref_position(cx, expr) {
                            self.state = Some((
                                State::Borrow,
                                StateData {
                                    span: expr.span,
                                    hir_id: expr.hir_id,
                                },
                            ));
                        }
                    },
                    _ => (),
                }
            },
            (
                Some((
                    State::DerefMethod {
                        target_mut,
                        ty_changed_count,
                        ..
                    },
                    data,
                )),
                RefOp::Method(_),
            ) => {
                self.state = Some((
                    State::DerefMethod {
                        ty_changed_count: if deref_method_same_type(typeck.expr_ty(expr), typeck.expr_ty(sub_expr)) {
                            ty_changed_count
                        } else {
                            ty_changed_count + 1
                        },
                        is_final_ufcs: matches!(expr.kind, ExprKind::Call(..)),
                        target_mut,
                    },
                    data,
                ));
            },
            (
                Some((
                    State::DerefedBorrow {
                        count,
                        required_precedence,
                        msg,
                    },
                    data,
                )),
                RefOp::AddrOf,
            ) if count != 0 => {
                self.state = Some((
                    State::DerefedBorrow {
                        count: count - 1,
                        required_precedence,
                        msg,
                    },
                    data,
                ));
            },
            (Some((State::Borrow, data)), RefOp::Deref) => {
                if typeck.expr_ty(sub_expr).is_ref() {
                    self.state = Some((
                        State::Reborrow {
                            deref_span: expr.span,
                            deref_hir_id: expr.hir_id,
                        },
                        data,
                    ));
                } else {
                    self.state = Some((
                        State::ExplicitDeref {
                            deref_span: expr.span,
                            deref_hir_id: expr.hir_id,
                        },
                        data,
                    ));
                }
            },
            (
                Some((
                    State::Reborrow {
                        deref_span,
                        deref_hir_id,
                    },
                    data,
                )),
                RefOp::Deref,
            ) => {
                self.state = Some((
                    State::ExplicitDeref {
                        deref_span,
                        deref_hir_id,
                    },
                    data,
                ));
            },
            (state @ Some((State::ExplicitDeref { .. }, _)), RefOp::Deref) => {
                self.state = state;
            },

            (Some((state, data)), _) => report(cx, expr, state, data),
        }
    }

    fn check_pat(&mut self, cx: &LateContext<'tcx>, pat: &'tcx Pat<'_>) {
        if let PatKind::Binding(BindingAnnotation::Ref, id, name, _) = pat.kind {
            if let Some(opt_prev_pat) = self.ref_locals.get_mut(&id) {
                // This binding id has been seen before. Add this pattern to the list of changes.
                if let Some(prev_pat) = opt_prev_pat {
                    if pat.span.from_expansion() {
                        // Doesn't match the context of the previous pattern. Can't lint here.
                        *opt_prev_pat = None;
                    } else {
                        prev_pat.spans.push(pat.span);
                        prev_pat.replacements.push((
                            pat.span,
                            snippet_with_context(cx, name.span, pat.span.ctxt(), "..", &mut prev_pat.app)
                                .0
                                .into(),
                        ));
                    }
                }
                return;
            }

            if_chain! {
                if !pat.span.from_expansion();
                if let ty::Ref(_, tam, _) = *cx.typeck_results().pat_ty(pat).kind();
                // only lint immutable refs, because borrowed `&mut T` cannot be moved out
                if let ty::Ref(_, _, Mutability::Not) = *tam.kind();
                then {
                    let mut app = Applicability::MachineApplicable;
                    let snip = snippet_with_context(cx, name.span, pat.span.ctxt(), "..", &mut app).0;
                    self.current_body = self.current_body.or(cx.enclosing_body);
                    self.ref_locals.insert(
                        id,
                        Some(RefPat {
                            always_deref: true,
                            spans: vec![pat.span],
                            app,
                            replacements: vec![(pat.span, snip.into())],
                            hir_id: pat.hir_id
                        }),
                    );
                }
            }
        }
    }

    fn check_body_post(&mut self, cx: &LateContext<'tcx>, body: &'tcx Body<'_>) {
        if Some(body.id()) == self.current_body {
            for pat in self.ref_locals.drain(..).filter_map(|(_, x)| x) {
                let replacements = pat.replacements;
                let app = pat.app;
                let lint = if pat.always_deref {
                    NEEDLESS_BORROW
                } else {
                    REF_BINDING_TO_REFERENCE
                };
                span_lint_hir_and_then(
                    cx,
                    lint,
                    pat.hir_id,
                    pat.spans,
                    "this pattern creates a reference to a reference",
                    |diag| {
                        diag.multipart_suggestion("try this", replacements, app);
                    },
                );
            }
            self.current_body = None;
        }
    }
}

fn try_parse_ref_op<'tcx>(
    tcx: TyCtxt<'tcx>,
    typeck: &'tcx TypeckResults<'_>,
    expr: &'tcx Expr<'_>,
) -> Option<(RefOp, &'tcx Expr<'tcx>)> {
    let (def_id, arg) = match expr.kind {
        ExprKind::MethodCall(_, [arg], _) => (typeck.type_dependent_def_id(expr.hir_id)?, arg),
        ExprKind::Call(
            Expr {
                kind: ExprKind::Path(path),
                hir_id,
                ..
            },
            [arg],
        ) => (typeck.qpath_res(path, *hir_id).opt_def_id()?, arg),
        ExprKind::Unary(UnOp::Deref, sub_expr) if !typeck.expr_ty(sub_expr).is_unsafe_ptr() => {
            return Some((RefOp::Deref, sub_expr));
        },
        ExprKind::AddrOf(BorrowKind::Ref, _, sub_expr) => return Some((RefOp::AddrOf, sub_expr)),
        _ => return None,
    };
    if tcx.is_diagnostic_item(sym::deref_method, def_id) {
        Some((RefOp::Method(Mutability::Not), arg))
    } else if tcx.trait_of_item(def_id)? == tcx.lang_items().deref_mut_trait()? {
        Some((RefOp::Method(Mutability::Mut), arg))
    } else {
        None
    }
}

// Checks whether the type for a deref call actually changed the type, not just the mutability of
// the reference.
fn deref_method_same_type<'tcx>(result_ty: Ty<'tcx>, arg_ty: Ty<'tcx>) -> bool {
    match (result_ty.kind(), arg_ty.kind()) {
        (ty::Ref(_, result_ty, _), ty::Ref(_, arg_ty, _)) => result_ty == arg_ty,

        // The result type for a deref method is always a reference
        // Not matching the previous pattern means the argument type is not a reference
        // This means that the type did change
        _ => false,
    }
}

// Checks whether the parent node is a suitable context for switching from a deref method to the
// deref operator.
fn is_linted_explicit_deref_position(parent: Option<Node<'_>>, child_id: HirId, child_span: Span) -> bool {
    let parent = match parent {
        Some(Node::Expr(e)) if e.span.ctxt() == child_span.ctxt() => e,
        _ => return true,
    };
    match parent.kind {
        // Leave deref calls in the middle of a method chain.
        // e.g. x.deref().foo()
        ExprKind::MethodCall(_, [self_arg, ..], _) if self_arg.hir_id == child_id => false,

        // Leave deref calls resulting in a called function
        // e.g. (x.deref())()
        ExprKind::Call(func_expr, _) if func_expr.hir_id == child_id => false,

        // Makes an ugly suggestion
        // e.g. *x.deref() => *&*x
        ExprKind::Unary(UnOp::Deref, _)
        // Postfix expressions would require parens
        | ExprKind::Match(_, _, MatchSource::TryDesugar | MatchSource::AwaitDesugar)
        | ExprKind::Field(..)
        | ExprKind::Index(..)
        | ExprKind::Err => false,

        ExprKind::Box(..)
        | ExprKind::ConstBlock(..)
        | ExprKind::Array(_)
        | ExprKind::Call(..)
        | ExprKind::MethodCall(..)
        | ExprKind::Tup(..)
        | ExprKind::Binary(..)
        | ExprKind::Unary(..)
        | ExprKind::Lit(..)
        | ExprKind::Cast(..)
        | ExprKind::Type(..)
        | ExprKind::DropTemps(..)
        | ExprKind::If(..)
        | ExprKind::Loop(..)
        | ExprKind::Match(..)
        | ExprKind::Let(..)
        | ExprKind::Closure{..}
        | ExprKind::Block(..)
        | ExprKind::Assign(..)
        | ExprKind::AssignOp(..)
        | ExprKind::Path(..)
        | ExprKind::AddrOf(..)
        | ExprKind::Break(..)
        | ExprKind::Continue(..)
        | ExprKind::Ret(..)
        | ExprKind::InlineAsm(..)
        | ExprKind::Struct(..)
        | ExprKind::Repeat(..)
        | ExprKind::Yield(..) => true,
    }
}

/// Checks if the given expression is in a position which can be auto-reborrowed.
/// Note: This is only correct assuming auto-deref is already occurring.
fn is_auto_reborrow_position(parent: Option<Node<'_>>) -> bool {
    match parent {
        Some(Node::Expr(parent)) => matches!(parent.kind, ExprKind::MethodCall(..) | ExprKind::Call(..)),
        Some(Node::Local(_)) => true,
        _ => false,
    }
}

/// Checks if the given expression is a position which can auto-borrow.
fn is_auto_borrow_position(parent: Option<Node<'_>>, child_id: HirId) -> bool {
    if let Some(Node::Expr(parent)) = parent {
        match parent.kind {
            // ExprKind::MethodCall(_, [self_arg, ..], _) => self_arg.hir_id == child_id,
            ExprKind::Field(..) => true,
            ExprKind::Call(f, _) => f.hir_id == child_id,
            _ => false,
        }
    } else {
        false
    }
}

/// Adjustments are sometimes made in the parent block rather than the expression itself.
fn find_adjustments<'tcx>(
    tcx: TyCtxt<'tcx>,
    typeck: &'tcx TypeckResults<'tcx>,
    expr: &'tcx Expr<'tcx>,
) -> &'tcx [Adjustment<'tcx>] {
    let map = tcx.hir();
    let mut iter = map.parent_iter(expr.hir_id);
    let mut prev = expr;

    loop {
        match typeck.expr_adjustments(prev) {
            [] => (),
            a => break a,
        };

        match iter.next().map(|(_, x)| x) {
            Some(Node::Block(_)) => {
                if let Some((_, Node::Expr(e))) = iter.next() {
                    prev = e;
                } else {
                    // This shouldn't happen. Blocks are always contained in an expression.
                    break &[];
                }
            },
            Some(Node::Expr(&Expr {
                kind: ExprKind::Break(Destination { target_id: Ok(id), .. }, _),
                ..
            })) => {
                if let Some(Node::Expr(e)) = map.find(id) {
                    prev = e;
                    iter = map.parent_iter(id);
                } else {
                    // This shouldn't happen. The destination should exist.
                    break &[];
                }
            },
            _ => break &[],
        }
    }
}

// Checks if the expression for the given id occurs in a position which auto dereferencing applies.
// Note that the target type must not be inferred in a way that may cause auto-deref to select a
// different type, nor may the position be the result of a macro expansion.
//
// e.g. the following should not linted
// macro_rules! foo { ($e:expr) => { let x: &str = $e; }}
// foo!(&*String::new());
// fn foo<T>(_: &T) {}
// foo(&*String::new())
fn is_stable_auto_deref_position<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'_>) -> bool {
    walk_to_expr_usage(cx, e, |node, child_id| match node {
        Node::Local(&Local { ty: Some(ty), .. }) => Some(is_binding_ty_auto_deref_stable(ty)),
        Node::Item(&Item {
            kind: ItemKind::Static(..) | ItemKind::Const(..),
            ..
        })
        | Node::TraitItem(&TraitItem {
            kind: TraitItemKind::Const(..),
            ..
        })
        | Node::ImplItem(&ImplItem {
            kind: ImplItemKind::Const(..),
            ..
        }) => Some(true),

        Node::Item(&Item {
            kind: ItemKind::Fn(..),
            def_id,
            ..
        })
        | Node::TraitItem(&TraitItem {
            kind: TraitItemKind::Fn(..),
            def_id,
            ..
        })
        | Node::ImplItem(&ImplItem {
            kind: ImplItemKind::Fn(..),
            def_id,
            ..
        }) => {
            let output = cx.tcx.fn_sig(def_id.to_def_id()).skip_binder().output();
            Some(!(output.has_placeholders() || output.has_opaque_types()))
        },

        Node::Expr(e) => match e.kind {
            ExprKind::Ret(_) => {
                let output = cx
                    .tcx
                    .fn_sig(cx.tcx.hir().body_owner_def_id(cx.enclosing_body.unwrap()))
                    .skip_binder()
                    .output();
                Some(!(output.has_placeholders() || output.has_opaque_types()))
            },
            ExprKind::Call(func, args) => Some(
                args.iter()
                    .position(|arg| arg.hir_id == child_id)
                    .zip(expr_sig(cx, func))
                    .and_then(|(i, sig)| sig.input_with_hir(i))
                    .map_or(false, |(hir_ty, ty)| match hir_ty {
                        // Type inference for closures can depend on how they're called. Only go by the explicit
                        // types here.
                        Some(ty) => is_binding_ty_auto_deref_stable(ty),
                        None => is_param_auto_deref_stable(ty.skip_binder()),
                    }),
            ),
            ExprKind::MethodCall(_, [_, args @ ..], _) => {
                let id = cx.typeck_results().type_dependent_def_id(e.hir_id).unwrap();
                Some(args.iter().position(|arg| arg.hir_id == child_id).map_or(false, |i| {
                    let arg = cx.tcx.fn_sig(id).skip_binder().inputs()[i + 1];
                    is_param_auto_deref_stable(arg)
                }))
            },
            ExprKind::Struct(path, fields, _) => {
                let variant = variant_of_res(cx, cx.qpath_res(path, e.hir_id));
                Some(
                    fields
                        .iter()
                        .find(|f| f.expr.hir_id == child_id)
                        .zip(variant)
                        .and_then(|(field, variant)| variant.fields.iter().find(|f| f.name == field.ident.name))
                        .map_or(false, |field| is_param_auto_deref_stable(cx.tcx.type_of(field.did))),
                )
            },
            _ => None,
        },
        _ => None,
    })
    .unwrap_or(false)
}

// Checks whether auto-dereferencing any type into a binding of the given type will definitely
// produce the same result.
//
// e.g.
// let x = Box::new(Box::new(0u32));
// let y1: &Box<_> = x.deref();
// let y2: &Box<_> = &x;
//
// Here `y1` and `y2` would resolve to different types, so the type `&Box<_>` is not stable when
// switching to auto-dereferencing.
fn is_binding_ty_auto_deref_stable(ty: &hir::Ty<'_>) -> bool {
    let (ty, count) = peel_hir_ty_refs(ty);
    if count != 1 {
        return false;
    }

    match &ty.kind {
        TyKind::Rptr(_, ty) => is_binding_ty_auto_deref_stable(ty.ty),
        &TyKind::Path(
            QPath::TypeRelative(_, path)
            | QPath::Resolved(
                _,
                Path {
                    segments: [.., path], ..
                },
            ),
        ) => {
            if let Some(args) = path.args {
                args.args.iter().all(|arg| {
                    if let GenericArg::Type(ty) = arg {
                        !ty_contains_infer(ty)
                    } else {
                        true
                    }
                })
            } else {
                true
            }
        },
        TyKind::Slice(_)
        | TyKind::Array(..)
        | TyKind::BareFn(_)
        | TyKind::Never
        | TyKind::Tup(_)
        | TyKind::Ptr(_)
        | TyKind::TraitObject(..)
        | TyKind::Path(_) => true,
        TyKind::OpaqueDef(..) | TyKind::Infer | TyKind::Typeof(..) | TyKind::Err => false,
    }
}

// Checks whether a type is inferred at some point.
// e.g. `_`, `Box<_>`, `[_]`
fn ty_contains_infer(ty: &hir::Ty<'_>) -> bool {
    match &ty.kind {
        TyKind::Slice(ty) | TyKind::Array(ty, _) => ty_contains_infer(ty),
        TyKind::Ptr(ty) | TyKind::Rptr(_, ty) => ty_contains_infer(ty.ty),
        TyKind::Tup(tys) => tys.iter().any(ty_contains_infer),
        TyKind::BareFn(ty) => {
            if ty.decl.inputs.iter().any(ty_contains_infer) {
                return true;
            }
            if let FnRetTy::Return(ty) = &ty.decl.output {
                ty_contains_infer(ty)
            } else {
                false
            }
        },
        &TyKind::Path(
            QPath::TypeRelative(_, path)
            | QPath::Resolved(
                _,
                Path {
                    segments: [.., path], ..
                },
            ),
        ) => {
            if let Some(args) = path.args {
                args.args.iter().any(|arg| {
                    if let GenericArg::Type(ty) = arg {
                        ty_contains_infer(ty)
                    } else {
                        false
                    }
                })
            } else {
                false
            }
        },
        TyKind::Path(_) | TyKind::OpaqueDef(..) | TyKind::Infer | TyKind::Typeof(_) | TyKind::Err => true,
        TyKind::Never | TyKind::TraitObject(..) => false,
    }
}

// Checks whether a type is stable when switching to auto dereferencing,
fn is_param_auto_deref_stable(ty: Ty<'_>) -> bool {
    let (ty, count) = peel_mid_ty_refs(ty);
    if count != 1 {
        return false;
    }

    match ty.kind() {
        ty::Bool
        | ty::Char
        | ty::Int(_)
        | ty::Uint(_)
        | ty::Float(_)
        | ty::Foreign(_)
        | ty::Str
        | ty::Array(..)
        | ty::Slice(..)
        | ty::RawPtr(..)
        | ty::FnDef(..)
        | ty::FnPtr(_)
        | ty::Closure(..)
        | ty::Generator(..)
        | ty::GeneratorWitness(..)
        | ty::Never
        | ty::Tuple(_)
        | ty::Ref(..)
        | ty::Projection(_) => true,
        ty::Infer(_)
        | ty::Error(_)
        | ty::Param(_)
        | ty::Bound(..)
        | ty::Opaque(..)
        | ty::Placeholder(_)
        | ty::Dynamic(..) => false,
        ty::Adt(..) => !(ty.has_placeholders() || ty.has_param_types_or_consts()),
    }
}

#[expect(clippy::needless_pass_by_value)]
fn report<'tcx>(cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>, state: State, data: StateData) {
    match state {
        State::DerefMethod {
            ty_changed_count,
            is_final_ufcs,
            target_mut,
        } => {
            let mut app = Applicability::MachineApplicable;
            let (expr_str, expr_is_macro_call) = snippet_with_context(cx, expr.span, data.span.ctxt(), "..", &mut app);
            let ty = cx.typeck_results().expr_ty(expr);
            let (_, ref_count) = peel_mid_ty_refs(ty);
            let deref_str = if ty_changed_count >= ref_count && ref_count != 0 {
                // a deref call changing &T -> &U requires two deref operators the first time
                // this occurs. One to remove the reference, a second to call the deref impl.
                "*".repeat(ty_changed_count + 1)
            } else {
                "*".repeat(ty_changed_count)
            };
            let addr_of_str = if ty_changed_count < ref_count {
                // Check if a reborrow from &mut T -> &T is required.
                if target_mut == Mutability::Not && matches!(ty.kind(), ty::Ref(_, _, Mutability::Mut)) {
                    "&*"
                } else {
                    ""
                }
            } else if target_mut == Mutability::Mut {
                "&mut "
            } else {
                "&"
            };

            let expr_str = if !expr_is_macro_call && is_final_ufcs && expr.precedence().order() < PREC_PREFIX {
                format!("({})", expr_str)
            } else {
                expr_str.into_owned()
            };

            span_lint_and_sugg(
                cx,
                EXPLICIT_DEREF_METHODS,
                data.span,
                match target_mut {
                    Mutability::Not => "explicit `deref` method call",
                    Mutability::Mut => "explicit `deref_mut` method call",
                },
                "try this",
                format!("{}{}{}", addr_of_str, deref_str, expr_str),
                app,
            );
        },
        State::DerefedBorrow {
            required_precedence,
            msg,
            ..
        } => {
            let mut app = Applicability::MachineApplicable;
            let snip = snippet_with_context(cx, expr.span, data.span.ctxt(), "..", &mut app).0;
            span_lint_hir_and_then(cx, NEEDLESS_BORROW, data.hir_id, data.span, msg, |diag| {
                let sugg = if required_precedence > expr.precedence().order() && !has_enclosing_paren(&snip) {
                    format!("({})", snip)
                } else {
                    snip.into()
                };
                diag.span_suggestion(data.span, "change this to", sugg, app);
            });
        },
        State::ExplicitDeref {
            deref_span,
            deref_hir_id,
        } => {
            let (span, hir_id) = if cx.typeck_results().expr_ty(expr).is_ref() {
                (data.span, data.hir_id)
            } else {
                (deref_span, deref_hir_id)
            };
            span_lint_hir_and_then(
                cx,
                EXPLICIT_AUTO_DEREF,
                hir_id,
                span,
                "deref which would be done by auto-deref",
                |diag| {
                    let mut app = Applicability::MachineApplicable;
                    let snip = snippet_with_context(cx, expr.span, span.ctxt(), "..", &mut app).0;
                    diag.span_suggestion(span, "try this", snip.into_owned(), app);
                },
            );
        },
        State::Borrow | State::Reborrow { .. } => (),
    }
}

impl Dereferencing {
    fn check_local_usage<'tcx>(&mut self, cx: &LateContext<'tcx>, e: &Expr<'tcx>, local: HirId) {
        if let Some(outer_pat) = self.ref_locals.get_mut(&local) {
            if let Some(pat) = outer_pat {
                // Check for auto-deref
                if !matches!(
                    cx.typeck_results().expr_adjustments(e),
                    [
                        Adjustment {
                            kind: Adjust::Deref(_),
                            ..
                        },
                        Adjustment {
                            kind: Adjust::Deref(_),
                            ..
                        },
                        ..
                    ]
                ) {
                    match get_parent_expr(cx, e) {
                        // Field accesses are the same no matter the number of references.
                        Some(Expr {
                            kind: ExprKind::Field(..),
                            ..
                        }) => (),
                        Some(&Expr {
                            span,
                            kind: ExprKind::Unary(UnOp::Deref, _),
                            ..
                        }) if !span.from_expansion() => {
                            // Remove explicit deref.
                            let snip = snippet_with_context(cx, e.span, span.ctxt(), "..", &mut pat.app).0;
                            pat.replacements.push((span, snip.into()));
                        },
                        Some(parent) if !parent.span.from_expansion() => {
                            // Double reference might be needed at this point.
                            if parent.precedence().order() == PREC_POSTFIX {
                                // Parentheses would be needed here, don't lint.
                                *outer_pat = None;
                            } else {
                                pat.always_deref = false;
                                let snip = snippet_with_context(cx, e.span, parent.span.ctxt(), "..", &mut pat.app).0;
                                pat.replacements.push((e.span, format!("&{}", snip)));
                            }
                        },
                        _ if !e.span.from_expansion() => {
                            // Double reference might be needed at this point.
                            pat.always_deref = false;
                            let snip = snippet_with_applicability(cx, e.span, "..", &mut pat.app);
                            pat.replacements.push((e.span, format!("&{}", snip)));
                        },
                        // Edge case for macros. The span of the identifier will usually match the context of the
                        // binding, but not if the identifier was created in a macro. e.g. `concat_idents` and proc
                        // macros
                        _ => *outer_pat = None,
                    }
                }
            }
        }
    }
}

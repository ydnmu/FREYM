//! Presentation context scoped to a subtree, including its lazy RenderOnce work.
//! A restored guard prevents settings density leaking to another window/overlay.
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};
use std::cell::Cell;

#[derive(Clone, Copy, Default)]
pub(crate) struct ControlStyle {
    pub settings: bool,
    pub inert: bool,
}
thread_local! { static CURRENT: Cell<ControlStyle> = const { Cell::new(ControlStyle { settings:false, inert:false }) }; }
pub(crate) fn current() -> ControlStyle {
    CURRENT.get()
}
fn scoped<R>(style: ControlStyle, run: impl FnOnce() -> R) -> R {
    struct Restore(ControlStyle);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.set(self.0);
        }
    }
    let previous = CURRENT.replace(style);
    let _restore = Restore(previous);
    run()
}
pub(crate) struct ControlScope {
    style: ControlStyle,
    child: AnyElement,
}
impl ControlScope {
    pub(crate) fn new(style: ControlStyle, child: impl IntoElement) -> Self {
        Self {
            style,
            child: child.into_any_element(),
        }
    }
}
impl IntoElement for ControlScope {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for ControlScope {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (
            scoped(self.style, || self.child.request_layout(window, cx)),
            (),
        )
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        scoped(self.style, || {
            self.child.prepaint(window, cx);
        });
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        scoped(self.style, || self.child.paint(window, cx));
    }
}

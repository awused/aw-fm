use std::rc::Rc;

use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::traits::{EventControllerExt, WidgetExt};

use super::Gui;

impl Gui {
    pub(super) fn setup_interaction(self: &Rc<Self>) {
        let dismiss_toast = gtk::GestureClick::new();

        dismiss_toast.connect_pressed(|gc, _n, _x, _y| {
            gc.widget().set_visible(false);
        });

        self.window.imp().toast.add_controller(dismiss_toast);
    }
}

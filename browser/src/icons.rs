use dioxus_native::prelude::*;

pub const REFRESH_ICON: &str = local_asset!("icons/rotate-cw.svg");
pub const HOME_ICON: &str = local_asset!("icons/house.svg");
pub const BACK_ICON: &str = local_asset!("icons/arrow-left.svg");
pub const FORWARDS_ICON: &str = local_asset!("icons/arrow-right.svg");
pub const MENU_ICON: &str = local_asset!("icons/ellipsis-vertical.svg");
pub const EXTERNAL_LINK_ICON: &str = local_asset!("icons/external-link.svg");
pub const CODE_ICON: &str = local_asset!("icons/code.svg");
#[cfg(any(feature = "screenshot", feature = "capture"))]
pub const CAMERA_ICON: &str = local_asset!("icons/camera.svg");

#[component]
pub fn IconButton(
    icon: &'static str,
    action: Option<Callback>,
    #[props(default)] active: bool,
    #[props(default)] disabled: bool,
) -> Element {
    let class = if disabled {
        "iconbutton iconbutton--disabled"
    } else if active {
        "iconbutton active"
    } else {
        "iconbutton"
    };
    rsx!(
        div {
            class,
            onclick: move |_| {
                if disabled {
                    return;
                }
                if let Some(action) = action {
                    action(())
                }
            },
            img { class: "urlbar-icon", src: icon }
        }
    )
}

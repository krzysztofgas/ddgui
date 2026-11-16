mod disks;
mod error;
mod format;
mod formats;
mod job;
mod pump;
mod ui;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{ ProtocolObject, Sel };
use objc2::sel;
use objc2_app_kit::{
    NSApplication,
    NSApplicationActivationPolicy,
    NSEventModifierFlags,
    NSMenu,
    NSMenuItem,
};
use objc2_foundation::{ NSProcessInfo, NSString };

fn main() {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("ddgui must run on the main thread");
        std::process::exit(1);
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.setMainMenu(Some(&main_menu(mtm)));
    let controller = ui::Controller::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
    app.run();
}

type Entry<'a> = Option<(&'a str, Sel, &'a str, NSEventModifierFlags)>;

fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let name = NSProcessInfo::processInfo().processName().to_string();
    let cmd = NSEventModifierFlags::Command;
    let cmd_opt = NSEventModifierFlags::Command | NSEventModifierFlags::Option;
    let cmd_shift = NSEventModifierFlags::Command | NSEventModifierFlags::Shift;
    let hide = format!("Hide {name}");
    let quit = format!("Quit {name}");
    let bar = NSMenu::new(mtm);
    let menus: [(&str, Vec<Entry<'_>>); 3] = [
        (
            &name,
            vec![
                Some((&hide, sel!(hide:), "h", cmd)),
                Some(("Hide Others", sel!(hideOtherApplications:), "h", cmd_opt)),
                Some(("Show All", sel!(unhideAllApplications:), "", cmd)),
                None,
                Some((&quit, sel!(terminate:), "q", cmd))
            ],
        ),
        (
            "Edit",
            vec![
                Some(("Undo", sel!(undo:), "z", cmd)),
                Some(("Redo", sel!(redo:), "z", cmd_shift)),
                None,
                Some(("Cut", sel!(cut:), "x", cmd)),
                Some(("Copy", sel!(copy:), "c", cmd)),
                Some(("Paste", sel!(paste:), "v", cmd)),
                Some(("Select All", sel!(selectAll:), "a", cmd))
            ],
        ),
        (
            "Window",
            vec![
                Some(("Minimize", sel!(performMiniaturize:), "m", cmd)),
                Some(("Close", sel!(performClose:), "w", cmd))
            ],
        ),
    ];
    for (title, entries) in menus {
        let menu = NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(title));
        for entry in entries {
            let item = match entry {
                None => NSMenuItem::separatorItem(mtm),
                Some((label, action, key, modifiers)) => {
                    let item = unsafe {
                        NSMenuItem::initWithTitle_action_keyEquivalent(
                            mtm.alloc(),
                            &NSString::from_str(label),
                            Some(action),
                            &NSString::from_str(key)
                        )
                    };
                    item.setKeyEquivalentModifierMask(modifiers);
                    item
                }
            };
            menu.addItem(&item);
        }
        let holder = NSMenuItem::new(mtm);
        holder.setSubmenu(Some(&menu));
        bar.addItem(&holder);
    }
    bar
}

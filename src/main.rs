mod disks;
mod format;
mod job;
mod ui;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::sel;
use objc2_app_kit::{ NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem };
use objc2_foundation::{ NSProcessInfo, NSString, ns_string };

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

fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let name = NSProcessInfo::processInfo().processName();
    let bar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    let entries: [(String, Option<objc2::runtime::Sel>, &NSString); 3] = [
        (format!("Hide {name}"), Some(sel!(hide:)), ns_string!("h")),
        ("Hide Others".to_string(), Some(sel!(hideOtherApplications:)), ns_string!("h")),
        (format!("Quit {name}"), Some(sel!(terminate:)), ns_string!("q")),
    ];
    for (i, (title, action, key)) in entries.iter().enumerate() {
        if i == 2 {
            app_menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                *action,
                key
            )
        };
        if i == 1 {
            item.setKeyEquivalentModifierMask(
                objc2_app_kit::NSEventModifierFlags::Command |
                    objc2_app_kit::NSEventModifierFlags::Option
            );
        }
        app_menu.addItem(&item);
    }
    app_item.setSubmenu(Some(&app_menu));
    bar.addItem(&app_item);

    let window_item = NSMenuItem::new(mtm);
    let window_menu = NSMenu::initWithTitle(mtm.alloc(), ns_string!("Window"));
    let close = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            ns_string!("Close"),
            Some(sel!(performClose:)),
            ns_string!("w")
        )
    };
    let minimize = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            ns_string!("Minimize"),
            Some(sel!(performMiniaturize:)),
            ns_string!("m")
        )
    };
    window_menu.addItem(&minimize);
    window_menu.addItem(&close);
    window_item.setSubmenu(Some(&window_menu));
    bar.addItem(&window_item);
    bar
}

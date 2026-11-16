use std::cell::{ Cell, RefCell };
use std::path::PathBuf;
use std::sync::{ Arc, Mutex };
use std::time::{ Duration, Instant };

use objc2::rc::Retained;
use objc2::runtime::{ AnyObject, NSObject, NSObjectProtocol };
use objc2::{ DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel };
use objc2_app_kit::{
    NSAlert,
    NSAlertFirstButtonReturn,
    NSAlertStyle,
    NSApplication,
    NSApplicationDelegate,
    NSApplicationTerminateReply,
    NSBackingStoreType,
    NSButton,
    NSColor,
    NSFont,
    NSFontWeightRegular,
    NSLayoutAttribute,
    NSLayoutConstraintOrientation,
    NSLineBreakMode,
    NSModalResponseOK,
    NSOpenPanel,
    NSPopUpButton,
    NSProgressIndicator,
    NSProgressIndicatorStyle,
    NSSavePanel,
    NSSegmentSwitchTracking,
    NSSegmentedControl,
    NSStackView,
    NSTextAlignment,
    NSTextField,
    NSUserInterfaceLayoutOrientation,
    NSView,
    NSWindow,
    NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{
    NSArray,
    NSEdgeInsets,
    NSNotification,
    NSPoint,
    NSRect,
    NSRunLoop,
    NSRunLoopCommonModes,
    NSSize,
    NSString,
    NSTimer,
    ns_string,
};

use crate::disks::{ self, Disk, Watch };
use crate::format;
use crate::job::{ self, Direction, Job, Outcome, Phase };

const WIDTH: f64 = 500.0;
const LABEL_WIDTH: f64 = 60.0;

pub struct Views {
    window: Retained<NSWindow>,
    mode: Retained<NSSegmentedControl>,
    image_field: Retained<NSTextField>,
    choose: Retained<NSButton>,
    devices: Retained<NSPopUpButton>,
    options: Retained<NSTextField>,
    progress: Retained<NSProgressIndicator>,
    status: Retained<NSTextField>,
    action: Retained<NSButton>,
}

#[derive(Default)]
pub struct Ivars {
    views: RefCell<Option<Views>>,
    disks: RefCell<Vec<Disk>>,
    disk_error: RefCell<Option<String>>,
    image: RefCell<Option<PathBuf>>,
    job: RefCell<Option<Job>>,
    direction: Cell<Option<Direction>>,
    message: RefCell<Option<String>>,
    watch: Arc<Mutex<Watch>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
}

define_class! {
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DDGUIController"]
    #[ivars = Ivars]
    pub struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSApplicationDelegate for Controller {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            self.build();
            disks::spawn_watcher(self.ivars().watch.clone());
            self.start_timer();
            self.refresh();
            let app = NSApplication::sharedApplication(self.mtm());
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window(&self, _app: &NSApplication) -> bool {
            true
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            if self.confirm_stop() {
                self.wait_for_job(Duration::from_secs(5));
                NSApplicationTerminateReply::TerminateNow
            } else {
                NSApplicationTerminateReply::TerminateCancel
            }
        }
    }

    unsafe impl NSWindowDelegate for Controller {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.confirm_stop()
        }
    }

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.poll_disks();
            self.poll_job();
        }

        #[unsafe(method(modeChanged:))]
        fn mode_changed(&self, _sender: Option<&AnyObject>) {
            self.ivars().image.replace(None);
            self.ivars().message.replace(None);
            self.refresh();
        }

        #[unsafe(method(deviceChanged:))]
        fn device_changed(&self, _sender: Option<&AnyObject>) {
            self.ivars().message.replace(None);
            self.refresh();
        }

        #[unsafe(method(choose:))]
        fn choose(&self, _sender: Option<&AnyObject>) {
            if let Some(path) = self.pick_image() {
                self.ivars().image.replace(Some(path));
                self.ivars().message.replace(None);
                self.refresh();
            }
        }

        #[unsafe(method(act:))]
        fn act(&self, _sender: Option<&AnyObject>) {
            if self.ivars().job.borrow().is_some() {
                if let Some(job) = self.ivars().job.borrow().as_ref() {
                    job.cancel();
                }
                self.refresh();
            } else {
                self.begin();
            }
        }
    }
}

impl Controller {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(Ivars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn direction(&self) -> Direction {
        let selected = self
            .ivars()
            .views.borrow()
            .as_ref()
            .map(|v| v.mode.selectedSegment())
            .unwrap_or(0);
        if selected == 1 {
            Direction::DiskToImage
        } else {
            Direction::ImageToDisk
        }
    }

    fn selected_disk(&self) -> Option<Disk> {
        let views = self.ivars().views.borrow();
        let index = views.as_ref()?.devices.indexOfSelectedItem();
        let disks = self.ivars().disks.borrow();
        usize
            ::try_from(index)
            .ok()
            .and_then(|i| disks.get(i).cloned())
    }

    fn build(&self) {
        let mtm = self.mtm();
        let target: &AnyObject = self.as_ref();

        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, 200.0)),
                NSWindowStyleMask::Titled |
                    NSWindowStyleMask::Closable |
                    NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(ns_string!("ddgui"));
        window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(self)));

        let labels = NSArray::from_retained_slice(
            &[NSString::from_str("Write Image"), NSString::from_str("Read Device")]
        );
        let mode = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &labels,
                NSSegmentSwitchTracking::SelectOne,
                Some(target),
                Some(sel!(modeChanged:)),
                mtm
            )
        };
        mode.setSelectedSegment(0);

        let image_field = NSTextField::labelWithString(ns_string!(""), mtm);
        image_field.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
        image_field.setContentCompressionResistancePriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal
        );
        image_field.setContentHuggingPriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal
        );
        let choose = unsafe {
            NSButton::buttonWithTitle_target_action(
                ns_string!("Choose…"),
                Some(target),
                Some(sel!(choose:)),
                mtm
            )
        };
        let image_row = row(mtm, &[&caption(mtm, "Image"), &image_field, &choose]);

        let devices = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), NSRect::ZERO, false);
        unsafe {
            devices.setTarget(Some(target));
            devices.setAction(Some(sel!(deviceChanged:)));
        }
        devices.setContentHuggingPriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal
        );
        let device_row = row(mtm, &[&caption(mtm, "Device"), &devices]);

        let options = NSTextField::textFieldWithString(ns_string!(""), mtm);
        options.setPlaceholderString(Some(ns_string!("Optional, e.g. bs=1m conv=sync")));
        options.setToolTip(Some(ns_string!("Extra dd operands, separated by spaces")));
        options.setContentHuggingPriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal
        );
        let options_row = row(mtm, &[&caption(mtm, "Options"), &options]);

        let progress = NSProgressIndicator::new(mtm);
        progress.setStyle(NSProgressIndicatorStyle::Bar);
        progress.setIndeterminate(false);
        progress.setMinValue(0.0);
        progress.setMaxValue(1.0);

        let status = NSTextField::labelWithString(ns_string!(""), mtm);
        status.setTextColor(Some(&NSColor::secondaryLabelColor()));
        status.setFont(
            Some(
                &NSFont::monospacedDigitSystemFontOfSize_weight(
                    NSFont::smallSystemFontSize(),
                    unsafe {
                        NSFontWeightRegular
                    }
                )
            )
        );
        status.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        status.setContentCompressionResistancePriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal
        );
        status.setContentHuggingPriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal
        );
        let action = unsafe {
            NSButton::buttonWithTitle_target_action(
                ns_string!("Start"),
                Some(target),
                Some(sel!(act:)),
                mtm
            )
        };
        action.setKeyEquivalent(ns_string!("\r"));
        let bottom_row = row(mtm, &[&status, &action]);

        let stack = NSStackView::new(mtm);
        stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
        stack.setAlignment(NSLayoutAttribute::CenterX);
        stack.setSpacing(14.0);
        stack.setEdgeInsets(NSEdgeInsets {
            top: 18.0,
            left: 20.0,
            bottom: 18.0,
            right: 20.0,
        });
        stack.addArrangedSubview(&mode);
        for view in [&**image_row, &**device_row, &**options_row, &**progress, &**bottom_row] {
            stack.addArrangedSubview(view);
            view.widthAnchor()
                .constraintEqualToAnchor_constant(&stack.widthAnchor(), -40.0)
                .setActive(true);
        }
        stack.setCustomSpacing_afterView(18.0, &mode);
        stack.widthAnchor().constraintEqualToConstant(WIDTH).setActive(true);

        window.setContentView(Some(&stack));
        window.center();
        window.makeKeyAndOrderFront(None);

        self.ivars().views.replace(
            Some(Views {
                window,
                mode,
                image_field,
                choose,
                devices,
                options,
                progress,
                status,
                action,
            })
        );
    }

    fn start_timer(&self) {
        let target: &AnyObject = self.as_ref();
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                0.2,
                target,
                sel!(tick:),
                None,
                true
            )
        };
        unsafe {
            NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        }
        self.ivars().timer.replace(Some(timer));
    }

    fn poll_disks(&self) {
        let latest = match self.ivars().watch.lock() {
            Ok(mut w) => w.latest.take(),
            Err(_) => Some(Err("Device monitor stopped".to_string())),
        };
        let Some(snapshot) = latest else {
            return;
        };
        let guard = self.ivars().views.borrow();
        let Some(views) = guard.as_ref() else {
            return;
        };
        let previous = self.selected_disk().map(|d| d.id);
        match snapshot {
            Ok(list) => {
                self.ivars().disk_error.replace(None);
                views.devices.removeAllItems();
                for disk in &list {
                    let title = format!(
                        "{} — {} ({})",
                        disk.name,
                        format::bytes(disk.size),
                        disk.id
                    );
                    views.devices.addItemWithTitle(&NSString::from_str(&title));
                    if let Some(item) = views.devices.lastItem() {
                        item.setToolTip(Some(&NSString::from_str(&disk.node())));
                    }
                }
                let index = previous
                    .and_then(|id| list.iter().position(|d| d.id == id))
                    .unwrap_or(0);
                self.ivars().disks.replace(list);
                if !self.ivars().disks.borrow().is_empty() {
                    views.devices.selectItemAtIndex(index as isize);
                }
            }
            Err(e) => {
                self.ivars().disk_error.replace(Some(e));
            }
        }
        drop(guard);
        self.refresh();
    }

    fn poll_job(&self) {
        let finished = {
            let job = self.ivars().job.borrow();
            let Some(job) = job.as_ref() else {
                return;
            };
            let Ok(status) = job.status.lock() else {
                return;
            };
            match &status.phase {
                Phase::Done(outcome) => Some((outcome.clone(), status.total, status.started)),
                _ => None,
            }
        };
        if let Some((outcome, total, started)) = finished {
            self.ivars().job.replace(None);
            let text = match outcome {
                Outcome::Finished => {
                    let elapsed = started.map(|s| s.elapsed()).unwrap_or_default();
                    self.attention();
                    format!("Done — {} in {}", format::bytes(total), format::duration(elapsed))
                }
                Outcome::Cancelled => "Cancelled".to_string(),
                Outcome::Failed(e) => {
                    self.attention();
                    format!("Error: {e}")
                }
            };
            self.ivars().message.replace(Some(text));
        }
        self.refresh();
    }

    fn attention(&self) {
        let app = NSApplication::sharedApplication(self.mtm());
        if !app.isActive() {
            app.requestUserAttention(
                objc2_app_kit::NSRequestUserAttentionType::InformationalRequest
            );
        }
    }

    fn refresh(&self) {
        let views = self.ivars().views.borrow();
        let Some(v) = views.as_ref() else {
            return;
        };
        let direction = self.direction();
        let busy = self.ivars().job.borrow().is_some();
        let has_disks = !self.ivars().disks.borrow().is_empty();

        if !has_disks && v.devices.numberOfItems() == 0 {
            v.devices.addItemWithTitle(ns_string!("No external devices"));
        }
        v.window.setDocumentEdited(busy);
        v.mode.setEnabled(!busy);
        v.choose.setEnabled(!busy);
        v.devices.setEnabled(!busy && has_disks);
        v.options.setEnabled(!busy);
        v.choose.setTitle(match direction {
            Direction::ImageToDisk => ns_string!("Choose…"),
            Direction::DiskToImage => ns_string!("Save As…"),
        });

        let image = self.ivars().image.borrow();
        match image.as_ref() {
            Some(path) => {
                v.image_field.setStringValue(&NSString::from_str(&path.to_string_lossy()));
                v.image_field.setTextColor(Some(&NSColor::labelColor()));
            }
            None => {
                v.image_field.setStringValue(ns_string!("None selected"));
                v.image_field.setTextColor(Some(&NSColor::tertiaryLabelColor()));
            }
        }
        let ready = image.is_some() && self.selected_disk().is_some();
        drop(image);

        let job = self.ivars().job.borrow();
        let (fraction, text, indeterminate) = match job.as_ref() {
            Some(job) => describe(job),
            None => {
                let text = self
                    .ivars()
                    .message.borrow()
                    .clone()
                    .or_else(|| self.ivars().disk_error.borrow().clone())
                    .unwrap_or_else(|| {
                        if !has_disks {
                            "Connect a USB drive or SD card".to_string()
                        } else if !ready {
                            match direction {
                                Direction::ImageToDisk => "Choose an image to write".to_string(),
                                Direction::DiskToImage => {
                                    "Choose where to save the image".to_string()
                                }
                            }
                        } else {
                            "Ready".to_string()
                        }
                    });
                (0.0, text, false)
            }
        };
        if v.progress.isIndeterminate() != indeterminate {
            v.progress.setIndeterminate(indeterminate);
            if indeterminate {
                unsafe {
                    v.progress.startAnimation(None);
                }
            } else {
                unsafe {
                    v.progress.stopAnimation(None);
                }
            }
        }
        v.progress.setDoubleValue(fraction);
        v.status.setStringValue(&NSString::from_str(&text));
        v.status.setToolTip(Some(&NSString::from_str(&text)));

        let cancelling = job.as_ref().is_some_and(Job::is_cancelled);
        v.action.setTitle(if busy { ns_string!("Cancel") } else { ns_string!("Start") });
        v.action.setEnabled(if busy { !cancelling } else { ready });
        v.action.setKeyEquivalent(if busy { ns_string!("\u{1b}") } else { ns_string!("\r") });
    }

    fn pick_image(&self) -> Option<PathBuf> {
        let mtm = self.mtm();
        let url = (match self.direction() {
            Direction::ImageToDisk => {
                let panel = NSOpenPanel::openPanel(mtm);
                panel.setCanChooseFiles(true);
                panel.setCanChooseDirectories(false);
                panel.setAllowsMultipleSelection(false);
                panel.setMessage(Some(ns_string!("Choose a disk image to write")));
                if panel.runModal() != NSModalResponseOK {
                    return None;
                }
                panel.URL()
            }
            Direction::DiskToImage => {
                let panel = NSSavePanel::savePanel(mtm);
                let name = self
                    .selected_disk()
                    .map(|d| format!("{}.img", d.name.replace(['/', ':'], "-")))
                    .unwrap_or_else(|| "disk.img".to_string());
                panel.setNameFieldStringValue(&NSString::from_str(&name));
                panel.setCanCreateDirectories(true);
                if panel.runModal() != NSModalResponseOK {
                    return None;
                }
                panel.URL()
            }
        })?;
        url.path().map(|p| PathBuf::from(p.to_string()))
    }

    fn begin(&self) {
        let direction = self.direction();
        let Some(disk) = self.selected_disk() else {
            self.show_error("No device selected");
            return;
        };
        let Some(image) = self.ivars().image.borrow().clone() else {
            self.show_error("No image selected");
            return;
        };
        let text = self
            .ivars()
            .views
            .borrow()
            .as_ref()
            .map(|v| v.options.stringValue().to_string())
            .unwrap_or_default();
        let args = match job::parse_args(&text) {
            Ok(args) => args,
            Err(e) => {
                self.show_error(&format!("Invalid options: {e}"));
                return;
            }
        };
        if direction == Direction::ImageToDisk {
            let file = image
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let confirmed = self.confirm(
                &format!("Erase “{}” ({})?", disk.name, disk.id),
                &format!(
                    "Everything on this {} device will be replaced with “{}”. This cannot be undone.",
                    format::bytes(disk.size),
                    file
                ),
                "Erase"
            );
            if !confirmed {
                return;
            }
        }
        match Job::start(direction, image, disk, args) {
            Ok(job) => {
                self.ivars().direction.set(Some(direction));
                self.ivars().message.replace(None);
                self.ivars().job.replace(Some(job));
            }
            Err(e) => {
                self.ivars().message.replace(Some(format!("Error: {e}")));
            }
        }
        self.refresh();
    }

    fn confirm(&self, message: &str, info: &str, button: &str) -> bool {
        let alert = NSAlert::new(self.mtm());
        alert.setAlertStyle(NSAlertStyle::Critical);
        alert.setMessageText(&NSString::from_str(message));
        alert.setInformativeText(&NSString::from_str(info));
        alert.addButtonWithTitle(&NSString::from_str(button));
        let cancel = alert.addButtonWithTitle(ns_string!("Cancel"));
        cancel.setKeyEquivalent(ns_string!("\u{1b}"));
        alert.runModal() == NSAlertFirstButtonReturn
    }

    fn show_error(&self, message: &str) {
        let alert = NSAlert::new(self.mtm());
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&NSString::from_str(message));
        alert.runModal();
    }

    fn confirm_stop(&self) -> bool {
        let job = self.ivars().job.borrow();
        let Some(job) = job.as_ref() else {
            return true;
        };
        if job.is_cancelled() {
            return true;
        }
        let stop = self.confirm(
            "Stop the copy in progress?",
            "The device may be left in an unusable state.",
            "Stop"
        );
        if stop {
            job.cancel();
        }
        stop
    }

    fn wait_for_job(&self, limit: Duration) {
        let job = self.ivars().job.borrow();
        let Some(job) = job.as_ref() else {
            return;
        };
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            let done = job.status
                .lock()
                .map(|s| matches!(s.phase, Phase::Done(_)))
                .unwrap_or(true);
            if done {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn describe(job: &Job) -> (f64, String, bool) {
    let Ok(s) = job.status.lock() else {
        return (0.0, "Working…".to_string(), true);
    };
    if job.is_cancelled() {
        return (0.0, "Cancelling…".to_string(), true);
    }
    match &s.phase {
        Phase::Authorizing => (0.0, "Waiting for authorization…".to_string(), true),
        Phase::Finishing => (1.0, "Flushing…".to_string(), true),
        Phase::Done(_) => (1.0, String::new(), false),
        Phase::Copying => {
            let fraction = if s.total > 0 { (s.done as f64) / (s.total as f64) } else { 0.0 };
            let mut text = format!("{} of {}", format::bytes(s.done), format::bytes(s.total));
            if let Some(started) = s.started {
                let secs = started.elapsed().as_secs_f64();
                if secs >= 1.0 && s.done > 0 {
                    let rate = (s.done as f64) / secs;
                    let left = (s.total.saturating_sub(s.done) as f64) / rate;
                    text.push_str(
                        &format!(
                            " · {}/s · {} left",
                            format::bytes(rate as u64),
                            format::duration(Duration::from_secs_f64(left))
                        )
                    );
                }
            }
            (fraction, text, false)
        }
    }
}

fn caption(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setAlignment(NSTextAlignment::Right);
    label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    label.widthAnchor().constraintEqualToConstant(LABEL_WIDTH).setActive(true);
    label
}

fn row(mtm: MainThreadMarker, views: &[&NSView]) -> Retained<NSStackView> {
    let stack = NSStackView::new(mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    stack.setAlignment(NSLayoutAttribute::FirstBaseline);
    stack.setSpacing(8.0);
    for view in views {
        stack.addArrangedSubview(view);
    }
    stack
}

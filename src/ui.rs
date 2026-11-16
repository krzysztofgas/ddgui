use std::cell::{ OnceCell, RefCell };
use std::path::PathBuf;
use std::sync::{ Arc, Mutex };
use std::time::{ Duration, Instant };

use objc2::rc::Retained;
use objc2::runtime::{ AnyObject, NSObject, NSObjectProtocol, ProtocolObject };
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
    NSRequestUserAttentionType,
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
use crate::error::lock;
use crate::format;
use crate::formats::{ Source, Target };
use crate::job::{ Args, Direction, Job, Outcome, Phase, Plan, Snapshot };

const WIDTH: f64 = 500.0;
const LABEL_WIDTH: f64 = 60.0;
const TICK: f64 = 0.2;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

struct Views {
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

#[derive(Clone, PartialEq)]
struct Rendered {
    status: String,
    fraction: f64,
    indeterminate: bool,
    busy: bool,
    cancelling: bool,
    ready: bool,
    direction: Direction,
    has_disks: bool,
    image: Option<PathBuf>,
}

#[derive(Default)]
struct Meter {
    sample: Option<(Instant, u64)>,
    rate: Option<f64>,
}

impl Meter {
    fn update(&mut self, bytes: u64) -> Option<f64> {
        let now = Instant::now();
        match self.sample {
            Some((at, prev)) if now.duration_since(at) >= Duration::from_millis(800) => {
                let secs = now.duration_since(at).as_secs_f64();
                let instant = (bytes.saturating_sub(prev) as f64) / secs;
                self.rate = Some(match self.rate {
                    Some(rate) => rate * 0.7 + instant * 0.3,
                    None => instant,
                });
                self.sample = Some((now, bytes));
            }
            None => {
                self.sample = Some((now, bytes));
            }
            _ => {}
        }
        self.rate.filter(|r| r.is_finite() && *r > 0.0)
    }
}

#[derive(Default)]
pub struct Ivars {
    views: OnceCell<Views>,
    disks: RefCell<Vec<Disk>>,
    disk_error: RefCell<Option<String>>,
    image: RefCell<Option<(PathBuf, String)>>,
    job: RefCell<Option<Job>>,
    message: RefCell<Option<String>>,
    meter: RefCell<Meter>,
    rendered: RefCell<Option<Rendered>>,
    watch: Arc<Mutex<Watch>>,
    timer: OnceCell<Retained<NSTimer>>,
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
            if let Err(e) = disks::spawn_watcher(self.ivars().watch.clone()) {
                self.ivars().disk_error.replace(Some(e.to_string()));
            }
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
            if !self.confirm_stop() {
                return NSApplicationTerminateReply::TerminateCancel;
            }
            if let Some(job) = self.ivars().job.borrow().as_ref() {
                job.wait(SHUTDOWN_GRACE);
            }
            NSApplicationTerminateReply::TerminateNow
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
            let Some(path) = self.pick_image() else {
                return;
            };
            let described = match self.direction() {
                Direction::ImageToDisk => Source::detect(&path).map(Source::label),
                Direction::DiskToImage => Ok(Target::from_path(&path).label()),
            };
            match described {
                Ok(label) => {
                    self.ivars().image.replace(Some((path, label.to_owned())));
                    self.ivars().message.replace(None);
                }
                Err(e) => {
                    self.ivars().image.replace(None);
                    self.ivars().message.replace(Some(format!("Error: {e}")));
                }
            }
            self.refresh();
        }

        #[unsafe(method(act:))]
        fn act(&self, _sender: Option<&AnyObject>) {
            let running = self.ivars().job.borrow().as_ref().map(Job::cancel).is_some();
            if running {
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

    fn views(&self) -> Option<&Views> {
        self.ivars().views.get()
    }

    fn direction(&self) -> Direction {
        match self.views().map(|v| v.mode.selectedSegment()) {
            Some(1) => Direction::DiskToImage,
            _ => Direction::ImageToDisk,
        }
    }

    fn selected_disk(&self) -> Option<Disk> {
        let index = self.views()?.devices.indexOfSelectedItem();
        let index = usize::try_from(index).ok()?;
        self.ivars().disks.borrow().get(index).cloned()
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
        window.setDelegate(Some(ProtocolObject::from_ref(self)));

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
        flexible(&image_field, true);
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
        flexible(&devices, false);
        let device_row = row(mtm, &[&caption(mtm, "Device"), &devices]);

        let options = NSTextField::textFieldWithString(ns_string!(""), mtm);
        options.setPlaceholderString(Some(ns_string!("Optional, e.g. bs=1m conv=sync")));
        options.setToolTip(Some(ns_string!("Extra dd operands, separated by spaces")));
        flexible(&options, false);
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
        flexible(&status, true);
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
        stack.setEdgeInsets(NSEdgeInsets { top: 18.0, left: 20.0, bottom: 18.0, right: 20.0 });
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

        let _ = self.ivars().views.set(Views {
            window,
            mode,
            image_field,
            choose,
            devices,
            options,
            progress,
            status,
            action,
        });
    }

    fn start_timer(&self) {
        let target: &AnyObject = self.as_ref();
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                TICK,
                target,
                sel!(tick:),
                None,
                true
            )
        };
        unsafe {
            NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        }
        let _ = self.ivars().timer.set(timer);
    }

    fn poll_disks(&self) {
        let Some(snapshot) = lock(&self.ivars().watch).latest.take() else {
            return;
        };
        let Some(views) = self.views() else {
            return;
        };
        match snapshot {
            Ok(list) => {
                let previous = self.selected_disk().map(|d| d.id);
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
                if list.is_empty() {
                    views.devices.addItemWithTitle(ns_string!("No external devices"));
                } else {
                    let index = previous
                        .and_then(|id| list.iter().position(|d| d.id == id))
                        .unwrap_or(0);
                    views.devices.selectItemAtIndex(index as isize);
                }
                self.ivars().disks.replace(list);
            }
            Err(e) => {
                self.ivars().disk_error.replace(Some(format!("Error: {e}")));
            }
        }
        self.refresh();
    }

    fn poll_job(&self) {
        let snapshot = match self.ivars().job.borrow().as_ref() {
            Some(job) => job.snapshot(),
            None => {
                return;
            }
        };
        if let Phase::Done(outcome) = &snapshot.phase {
            self.ivars().job.replace(None);
            self.ivars().meter.replace(Meter::default());
            let text = match outcome {
                Outcome::Finished =>
                    format!(
                        "Done — {} in {}",
                        format::bytes(snapshot.written),
                        format::duration(snapshot.elapsed.unwrap_or_default())
                    ),
                Outcome::Cancelled => "Cancelled".to_owned(),
                Outcome::Failed(e) => format!("Error: {e}"),
            };
            if *outcome != Outcome::Cancelled {
                self.attention();
            }
            self.ivars().message.replace(Some(text));
        }
        self.refresh();
    }

    fn attention(&self) {
        let app = NSApplication::sharedApplication(self.mtm());
        if !app.isActive() {
            app.requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
        }
    }

    fn compute(&self) -> Rendered {
        let direction = self.direction();
        let has_disks = !self.ivars().disks.borrow().is_empty();
        let image = self.ivars().image.borrow().clone();
        let ready = image.is_some() && self.selected_disk().is_some();
        let job = self.ivars().job.borrow();
        let (fraction, status, indeterminate, cancelling) = match job.as_ref() {
            Some(job) => {
                let snapshot = job.snapshot();
                let rate = self.ivars().meter.borrow_mut().update(snapshot.written);
                let (fraction, text, indeterminate) = describe(&snapshot, rate);
                (fraction, text, indeterminate, snapshot.cancelled)
            }
            None => {
                let text = self
                    .ivars()
                    .message.borrow()
                    .clone()
                    .or_else(|| self.ivars().disk_error.borrow().clone())
                    .unwrap_or_else(|| {
                        match (&image, has_disks, ready) {
                            (_, false, _) => "Connect a USB drive or SD card".to_owned(),
                            (Some((_, label)), _, true) => format!("Ready · {label}"),
                            _ =>
                                match direction {
                                    Direction::ImageToDisk =>
                                        "Choose an image or folder to write".to_owned(),
                                    Direction::DiskToImage =>
                                        "Choose where to save the image".to_owned(),
                                }
                        }
                    });
                (0.0, text, false, false)
            }
        };
        Rendered {
            status,
            fraction,
            indeterminate,
            busy: job.is_some(),
            cancelling,
            ready,
            direction,
            has_disks,
            image: image.map(|(p, _)| p),
        }
    }

    fn refresh(&self) {
        let Some(v) = self.views() else {
            return;
        };
        let next = self.compute();
        let previous = self.ivars().rendered.replace(Some(next.clone()));
        if previous.as_ref() == Some(&next) {
            return;
        }
        let busy = next.busy;
        v.window.setDocumentEdited(busy);
        v.mode.setEnabled(!busy);
        v.choose.setEnabled(!busy);
        v.devices.setEnabled(!busy && next.has_disks);
        v.options.setEnabled(!busy);
        v.choose.setTitle(match next.direction {
            Direction::ImageToDisk => ns_string!("Choose…"),
            Direction::DiskToImage => ns_string!("Save As…"),
        });
        match &next.image {
            Some(path) => {
                let text = NSString::from_str(&path.to_string_lossy());
                v.image_field.setStringValue(&text);
                v.image_field.setToolTip(Some(&text));
                v.image_field.setTextColor(Some(&NSColor::labelColor()));
            }
            None => {
                v.image_field.setStringValue(ns_string!("None selected"));
                v.image_field.setToolTip(None);
                v.image_field.setTextColor(Some(&NSColor::tertiaryLabelColor()));
            }
        }
        if v.progress.isIndeterminate() != next.indeterminate {
            v.progress.setIndeterminate(next.indeterminate);
            unsafe {
                if next.indeterminate {
                    v.progress.startAnimation(None);
                } else {
                    v.progress.stopAnimation(None);
                }
            }
        }
        v.progress.setDoubleValue(next.fraction);
        let status = NSString::from_str(&next.status);
        v.status.setStringValue(&status);
        v.status.setToolTip(Some(&status));
        v.action.setTitle(if busy { ns_string!("Cancel") } else { ns_string!("Start") });
        v.action.setEnabled(if busy { !next.cancelling } else { next.ready });
        v.action.setKeyEquivalent(if busy { ns_string!("\u{1b}") } else { ns_string!("\r") });
    }

    fn pick_image(&self) -> Option<PathBuf> {
        let mtm = self.mtm();
        let url = (match self.direction() {
            Direction::ImageToDisk => {
                let panel = NSOpenPanel::openPanel(mtm);
                panel.setCanChooseFiles(true);
                panel.setCanChooseDirectories(true);
                panel.setAllowsMultipleSelection(false);
                panel.setMessage(
                    Some(
                        ns_string!(
                            "Choose an image (.img, .iso, .dmg, .zip, .gz, .xz, .zst, .bz2) or a folder"
                        )
                    )
                );
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
                    .unwrap_or_else(|| "disk.img".to_owned());
                panel.setNameFieldStringValue(&NSString::from_str(&name));
                panel.setCanCreateDirectories(true);
                panel.setMessage(
                    Some(
                        ns_string!("End the name with .gz, .xz, .zst or .bz2 to compress the image")
                    )
                );
                if panel.runModal() != NSModalResponseOK {
                    return None;
                }
                panel.URL()
            }
        })?;
        url.path().map(|p| PathBuf::from(p.to_string()))
    }

    fn begin(&self) {
        let Some(disk) = self.selected_disk() else {
            self.show_error("No device selected", None);
            return;
        };
        let Some((image, _)) = self.ivars().image.borrow().clone() else {
            self.show_error("No image selected", None);
            return;
        };
        let text = self
            .views()
            .map(|v| v.options.stringValue().to_string())
            .unwrap_or_default();
        let args = match Args::parse(&text) {
            Ok(args) => args,
            Err(e) => {
                self.show_error("Invalid options", Some(&e.to_string()));
                return;
            }
        };
        let direction = self.direction();
        let plan = match Plan::prepare(direction, image, &disk, args) {
            Ok(plan) => plan,
            Err(e) => {
                self.show_error("Can't start", Some(&e.to_string()));
                return;
            }
        };
        if direction == Direction::ImageToDisk && !self.confirm_erase(&plan) {
            return;
        }
        match Job::start(plan) {
            Ok(job) => {
                self.ivars().message.replace(None);
                self.ivars().meter.replace(Meter::default());
                self.ivars().job.replace(Some(job));
            }
            Err(e) => {
                self.ivars().message.replace(Some(format!("Error: {e}")));
            }
        }
        self.refresh();
    }

    fn confirm_erase(&self, plan: &Plan) -> bool {
        let disk = plan.disk();
        let file = plan
            .image()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let alert = NSAlert::new(self.mtm());
        alert.setAlertStyle(NSAlertStyle::Critical);
        alert.setMessageText(&NSString::from_str(&format!("Erase “{}” ({})?", disk.name, disk.id)));
        alert.setInformativeText(
            &NSString::from_str(
                &format!(
                    "Everything on this {} device will be replaced with “{file}” ({}). This cannot be undone.",
                    format::bytes(disk.size),
                    plan.description()
                )
            )
        );
        let erase = alert.addButtonWithTitle(ns_string!("Erase"));
        let cancel = alert.addButtonWithTitle(ns_string!("Cancel"));
        erase.setKeyEquivalent(ns_string!(""));
        cancel.setKeyEquivalent(ns_string!("\r"));
        alert.runModal() == NSAlertFirstButtonReturn
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

    fn show_error(&self, message: &str, info: Option<&str>) {
        let alert = NSAlert::new(self.mtm());
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&NSString::from_str(message));
        if let Some(info) = info {
            alert.setInformativeText(&NSString::from_str(info));
        }
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
}

fn describe(s: &Snapshot, rate: Option<f64>) -> (f64, String, bool) {
    if s.cancelled {
        return (0.0, "Cancelling…".to_owned(), true);
    }
    let fraction = if s.total > 0 {
        ((s.done as f64) / (s.total as f64)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    match &s.phase {
        Phase::Preparing if fraction > 0.0 => {
            (fraction, format!("Preparing image… {:.0}%", fraction * 100.0), false)
        }
        Phase::Preparing => (0.0, "Preparing image…".to_owned(), true),
        Phase::Authorizing => (0.0, "Waiting for authorization…".to_owned(), true),
        Phase::Finishing => (1.0, "Flushing…".to_owned(), true),
        Phase::Done(_) => (1.0, String::new(), false),
        Phase::Copying => {
            let mut text = if s.stream {
                format!("{:.0}% · {} written", fraction * 100.0, format::bytes(s.written))
            } else {
                format!("{} of {}", format::bytes(s.done.min(s.total)), format::bytes(s.total))
            };
            if let Some(rate) = rate {
                text.push_str(&format!(" · {}/s", format::bytes(rate as u64)));
                let elapsed = s.elapsed.unwrap_or_default().as_secs_f64();
                if fraction > 0.01 && elapsed > 2.0 {
                    let left = (elapsed * (1.0 - fraction)) / fraction;
                    text.push_str(
                        &format!(" · {} left", format::duration(Duration::from_secs_f64(left)))
                    );
                }
            }
            (fraction, text, false)
        }
    }
}

fn flexible(view: &NSView, compressible: bool) {
    view.setContentHuggingPriority_forOrientation(1.0, NSLayoutConstraintOrientation::Horizontal);
    if compressible {
        view.setContentCompressionResistancePriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal
        );
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

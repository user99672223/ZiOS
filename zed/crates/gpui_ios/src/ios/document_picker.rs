//! The Files app picker for opening folders and files (ZiOS, CLAUDE.md §14).
//!
//! Picked locations are security-scoped: access is started and kept for the
//! rest of the session so the editor can read and write them, and a bookmark is
//! stored so the same folder can be reopened on the next launch.

use std::{cell::RefCell, path::PathBuf};

use futures::channel::oneshot;
use objc2::{
    ClassType as _, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Allocated, Retained},
    runtime::{AnyClass, AnyObject, Bool, NSObject},
};
use objc2_foundation::{NSArray, NSObjectProtocol, NSString, NSURL};
use objc2_ui_kit::UIViewController;

type Reply = oneshot::Sender<anyhow::Result<Option<Vec<PathBuf>>>>;

pub(super) struct PickerIvars {
    reply: RefCell<Option<Reply>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIIosDocumentPickerDelegate"]
    #[ivars = PickerIvars]
    pub(super) struct PickerDelegate;

    unsafe impl NSObjectProtocol for PickerDelegate {}

    impl PickerDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, _picker: &AnyObject, urls: &NSArray<NSURL>) {
            let paths = urls
                .iter()
                .filter_map(|url| {
                    // Keep access for the session; iOS revokes it at exit.
                    let _: bool = unsafe { msg_send![&*url, startAccessingSecurityScopedResource] };
                    remember_bookmark(&url);
                    url.path().map(|path| PathBuf::from(path.to_string()))
                })
                .collect::<Vec<_>>();
            log::info!("GPUI iOS: picked {paths:?}");
            self.finish(Ok(Some(paths)));
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn cancelled(&self, _picker: &AnyObject) {
            self.finish(Ok(None));
        }
    }
);

impl PickerDelegate {
    fn new(reply: Reply, main_thread: MainThreadMarker) -> Retained<Self> {
        let this: Allocated<Self> = Self::alloc(main_thread);
        let this = this.set_ivars(PickerIvars {
            reply: RefCell::new(Some(reply)),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn finish(&self, result: anyhow::Result<Option<Vec<PathBuf>>>) {
        if let Some(reply) = self.ivars().reply.borrow_mut().take() {
            reply.send(result).ok();
        }
        DELEGATE.with_borrow_mut(|delegate| delegate.take());
    }
}

thread_local! {
    // UIKit holds its delegate weakly; keep it alive while the picker is shown.
    static DELEGATE: RefCell<Option<Retained<PickerDelegate>>> = const { RefCell::new(None) };
}

const BOOKMARKS_KEY: &str = "ZiOSFolderBookmarks";

/// Stores a bookmark so the folder stays reachable after relaunch.
fn remember_bookmark(url: &NSURL) {
    let bookmark: Option<Retained<AnyObject>> = unsafe {
        msg_send![url, bookmarkDataWithOptions: 0usize, includingResourceValuesForKeys: std::ptr::null::<AnyObject>(), relativeToURL: std::ptr::null::<AnyObject>(), error: std::ptr::null_mut::<*mut AnyObject>()]
    };
    let (Some(bookmark), Some(path)) = (bookmark, url.path()) else {
        return;
    };
    unsafe {
        let Some(defaults_class) = AnyClass::get(c"NSUserDefaults") else {
            return;
        };
        let defaults: Retained<AnyObject> = msg_send![defaults_class, standardUserDefaults];
        let key = NSString::from_str(BOOKMARKS_KEY);
        let existing: Option<Retained<AnyObject>> = msg_send![&*defaults, dictionaryForKey: &*key];
        let Some(dictionary_class) = AnyClass::get(c"NSMutableDictionary") else {
            return;
        };
        let dictionary: Retained<AnyObject> = match existing {
            Some(existing) => msg_send![dictionary_class, dictionaryWithDictionary: &*existing],
            None => msg_send![dictionary_class, dictionary],
        };
        let () = msg_send![&*dictionary, setObject: &*bookmark, forKey: &*path];
        let () = msg_send![&*defaults, setObject: &*dictionary, forKey: &*key];
    }
}

/// Re-opens access to folders picked in earlier sessions. Call at launch.
pub fn restore_bookmarked_folders() -> Vec<PathBuf> {
    let mut restored = Vec::new();
    unsafe {
        let Some(defaults_class) = AnyClass::get(c"NSUserDefaults") else {
            return restored;
        };
        let defaults: Retained<AnyObject> = msg_send![defaults_class, standardUserDefaults];
        let key = NSString::from_str(BOOKMARKS_KEY);
        let existing: Option<Retained<AnyObject>> = msg_send![&*defaults, dictionaryForKey: &*key];
        let Some(dictionary) = existing else {
            return restored;
        };
        let values: Retained<NSArray<AnyObject>> = msg_send![&*dictionary, allValues];
        for bookmark in values.iter() {
            let mut stale = Bool::NO;
            let url: Option<Retained<NSURL>> = msg_send![
                NSURL::class(),
                URLByResolvingBookmarkData: &*bookmark,
                options: 0usize,
                relativeToURL: std::ptr::null::<AnyObject>(),
                bookmarkDataIsStale: &mut stale,
                error: std::ptr::null_mut::<*mut AnyObject>()
            ];
            if let Some(url) = url {
                let _: bool = msg_send![&*url, startAccessingSecurityScopedResource];
                if let Some(path) = url.path() {
                    restored.push(PathBuf::from(path.to_string()));
                }
            }
        }
    }
    restored
}

/// Presents the picker over `presenter`; resolves with the chosen paths.
pub(super) fn present(
    presenter: &UIViewController,
    directories: bool,
    multiple: bool,
    reply: Reply,
) {
    let main_thread = MainThreadMarker::from(presenter);
    let Some(picker_class) = AnyClass::get(c"UIDocumentPickerViewController") else {
        reply.send(Err(anyhow::anyhow!("UIDocumentPickerViewController is unavailable"))).ok();
        return;
    };
    let type_identifier = NSString::from_str(if directories { "public.folder" } else { "public.item" });
    let types = NSArray::from_retained_slice(&[type_identifier]);
    // UIDocumentPickerModeOpen = 1: work on the original, not a copy.
    let picker: Retained<UIViewController> = unsafe {
        let allocated: Allocated<UIViewController> = msg_send![picker_class, alloc];
        msg_send![allocated, initWithDocumentTypes: &*types, inMode: 1usize]
    };
    let delegate = PickerDelegate::new(reply, main_thread);
    unsafe {
        let () = msg_send![&*picker, setDelegate: &*delegate];
        let () = msg_send![&*picker, setAllowsMultipleSelection: multiple];
    }
    DELEGATE.with_borrow_mut(|slot| *slot = Some(delegate));
    presenter.presentViewController_animated_completion(&picker, true, None);
}

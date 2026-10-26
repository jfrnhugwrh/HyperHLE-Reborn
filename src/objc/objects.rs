/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Handling of Objective-C objects.

use super::classes::{ClassHostObject, FakeClass, UnimplementedClass};
use super::Class;
use crate::mem::{guest_size_of, GuestUSize, Mem, MutPtr, Ptr, SafeRead};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Mutex;

/// Per-(id, TypeId) cache of phantom host-object buffers used when a
/// `borrow`/`borrow_mut` call hits an object that has no real host-side
/// record. Each entry is a zero-initialised, leaked buffer large enough to
/// hold `T`; callers get a stable reference that isn't aliased with buffers
/// for other objects/types.
///
/// The cache is behind a single process-wide `Mutex` because touchHLE keeps
/// a single `ObjC` instance but this function is used from both immutable
/// (`&self`) and mutable (`&mut self`) receivers, and from many framework
/// modules. Contention here is only hit on the error path, so a plain
/// `Mutex` is fine.
static PHANTOM_STORE: Mutex<Option<HashMap<(TypeId, usize), usize>>> = Mutex::new(None);

/// Number of deallocated object addresses remembered for use-after-free
/// diagnostics. Bounded so a churning app cannot grow the table without limit;
/// old enough dangling pointers simply report as "unknown" like they used to.
const FREED_OBJECT_MEMORY: usize = 4096;

fn phantom_buffer_for<T: 'static>(object: id, init: impl FnOnce() -> T) -> *mut u8 {
    let key = (TypeId::of::<T>(), object.to_bits() as usize);
    let mut guard = PHANTOM_STORE.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(&ptr) = map.get(&key) {
        return ptr as *mut u8;
    }
    // Leak a buffer sized and aligned for T, then write a real `T` value
    // into it. Using raw `alloc_zeroed` plus `transmute` (the previous
    // behaviour) was unsound for types whose zero bit-pattern is not a
    // valid instance — most notably anything containing a `HashMap`, whose
    // internal `ctrl` pointer must point at hashbrown's static empty
    // sentinel rather than null. Performing a proper `T::default()`
    // (passed in by the caller) ensures the buffer holds a usable
    // instance even on this error path.
    let layout = std::alloc::Layout::new::<T>();
    // SAFETY: `layout.size()` is non-zero for any real host object, and
    // the allocator returns a pointer with `layout.align()` alignment.
    // Writing `init()` (a `T` value) into freshly allocated, uninitialised
    // memory of exactly that layout is well-defined.
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null(), "phantom host object allocation failed");
    unsafe { std::ptr::write(ptr as *mut T, init()) };
    map.insert(key, ptr as usize);
    ptr
}

/// Return a `&T` pointing at a stable backing buffer for the given
/// missing-object id, initialised on first access via [`Default::default`].
/// Repeated calls with the same `object` and `T` return the same buffer
/// (which may have been mutated through [`phantom_host_object_mut`] in the
/// meantime).
fn phantom_host_object<T: Default + 'static>(object: id) -> &'static T {
    let ptr = phantom_buffer_for::<T>(object, T::default) as *const T;
    // SAFETY: `phantom_buffer_for` returns a stable allocation of
    // `size_of::<T>()` bytes with the required alignment, initialised on
    // first call via `T::default()`. Subsequent calls return the same
    // region, giving a stable `'static` reference to a valid `T`.
    unsafe { &*ptr }
}

/// Return a `&mut T` pointing at a stable backing buffer for the given
/// missing-object id, initialised on first access via [`Default::default`].
/// Repeated calls with the same `object` and `T` return a reference to the
/// same buffer.
fn phantom_host_object_mut<T: Default + 'static>(object: id) -> &'static mut T {
    let ptr = phantom_buffer_for::<T>(object, T::default) as *mut T;
    // SAFETY: See `phantom_host_object`. Additionally, because the cache
    // keys on `(TypeId, object)` each call-site gets an isolated buffer,
    // so mutations by one fake-borrow won't be visible to another fake-
    // borrow of a different object or type.
    unsafe { &mut *ptr }
}

/// Describe a host object for diagnostics, including the class name carried by
/// placeholder host objects.
///
/// `type_name()` alone was not enough: `UnimplementedClass` and `FakeClass`
/// both print as their Rust type, so a log line could say "requested
/// ClassHostObject, actual host type UnimplementedClass" without naming the
/// ObjC class, which made the mismatch (PlayHaven/Crittercism stubs in
/// practice) impossible to trace back to a class.
fn host_object_description(host_object: &(dyn AnyHostObject + 'static)) -> String {
    let any = host_object.as_any();
    if let Some(unimplemented) = any.downcast_ref::<UnimplementedClass>() {
        return format!(
            "UnimplementedClass(\"{}\", is_metaclass={})",
            unimplemented.name, unimplemented.is_metaclass
        );
    }
    if let Some(fake) = any.downcast_ref::<FakeClass>() {
        return format!(
            "FakeClass(\"{}\", is_metaclass={})",
            fake.name, fake.is_metaclass
        );
    }
    if let Some(class) = any.downcast_ref::<ClassHostObject>() {
        return format!(
            "ClassHostObject(\"{}\", is_metaclass={})",
            class.name, class.is_metaclass
        );
    }
    host_object.type_name().to_string()
}

#[repr(C, packed)]
pub struct objc_object {
    pub(super) isa: Class,
}
unsafe impl SafeRead for objc_object {}

#[allow(non_camel_case_types)]
pub type id = MutPtr<objc_object>;

#[allow(non_upper_case_globals)]
pub const nil: id = Ptr::null();

pub(super) struct HostObjectEntry {
    host_object: Box<dyn AnyHostObject>,
    refcount: Option<NonZeroU32>,
    /// The class the object was allocated as, when the runtime created it.
    ///
    /// Apple's runtime reads the class back out of the object's `isa` slot on
    /// every dispatch, which is why guest code that clobbers `isa` is a
    /// use-after-free / memory-corruption bug. Keeping the authoritative value
    /// host-side lets the dispatcher (a) name the class in diagnostics even
    /// after the guest pointer has been scribbled over, and (b) repair an
    /// `isa` slot that was zeroed by non-ObjC code without guessing. `None`
    /// for objects registered from guest memory (`register_static_object`),
    /// whose class is whatever the guest binary says it is.
    isa: Option<Class>,
}

pub trait HostObject: Any + 'static {
    fn as_superclass<'a>(&'a self) -> Option<&'a (dyn AnyHostObject + 'static)> {
        None
    }
    fn as_superclass_mut<'a>(&'a mut self) -> Option<&'a mut (dyn AnyHostObject + 'static)> {
        None
    }
}

#[macro_export]
macro_rules! impl_HostObject_with_superclass {
    ( $ty:ty ) => {
        impl $crate::objc::HostObject for $ty {
            fn as_superclass<'a>(
                &'a self,
            ) -> Option<&'a (dyn $crate::objc::AnyHostObject + 'static)> {
                Some(&self.superclass)
            }
            fn as_superclass_mut<'a>(
                &'a mut self,
            ) -> Option<&'a mut (dyn $crate::objc::AnyHostObject + 'static)> {
                Some(&mut self.superclass)
            }
        }
    };
}
pub use crate::impl_HostObject_with_superclass;

pub trait AnyHostObject: HostObject {
    fn as_any<'a>(&'a self) -> &'a (dyn Any + 'static);
    fn as_any_mut<'a>(&'a mut self) -> &'a mut (dyn Any + 'static);
    fn type_name(&self) -> &'static str;
}
impl<T: HostObject> AnyHostObject for T {
    fn as_any<'a>(&'a self) -> &'a (dyn Any + 'static) {
        self
    }
    fn as_any_mut<'a>(&'a mut self) -> &'a mut (dyn Any + 'static) {
        self
    }
    fn type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }
}

pub struct TrivialHostObject;
impl HostObject for TrivialHostObject {}

impl super::ObjC {
    pub fn read_isa(object: id, mem: &Mem) -> Class {
        if object == nil {
            return Ptr::null();
        }
        mem.read(object).isa
    }

    /// Fallible variant of [Self::read_isa].
    ///
    /// `read_isa` goes through `Mem::read`, which panics (and therefore kills
    /// the emulator) when the pointer does not address readable guest memory.
    /// Every pointer that arrives in the dispatcher is guest-controlled and may
    /// be wild — a dangling object address that the allocator has since
    /// unmapped, a `nil`+offset field read, or garbage from a corrupt
    /// structure. `None` here means "not even readable", which the caller
    /// reports as a corrupt pointer instead of taking the process down.
    pub(crate) fn try_read_isa(object: id, mem: &Mem) -> Option<Class> {
        if object == nil {
            return None;
        }
        let bytes = mem.get_bytes_fallible(
            object.cast_const().cast(),
            guest_size_of::<objc_object>(),
        )?;
        let isa_bits = u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?);
        Some(MutPtr::from_bits(isa_bits))
    }

    fn alloc_object_inner(
        &mut self,
        isa: Class,
        instance_size: GuestUSize,
        host_object: Box<dyn AnyHostObject>,
        mem: &mut Mem,
        refcount: Option<NonZeroU32>,
    ) -> id {
        let guest_object = objc_object { isa };
        let ptr: MutPtr<objc_object> = mem.alloc(instance_size).cast();
        mem.write(ptr, guest_object);
        // The guest allocator can hand out the address of a previously
        // deallocated object. Drop any stale freed-object record first so a
        // use-after-free report can never point at the wrong class.
        self.forget_freed_object(ptr);
        self.objects.insert(
            ptr,
            HostObjectEntry {
                host_object,
                refcount,
                isa: Some(isa),
            },
        );
        ptr
    }

    pub fn alloc_object(
        &mut self,
        isa: Class,
        host_object: Box<dyn AnyHostObject>,
        mem: &mut Mem,
    ) -> id {
        let instance_size = self
            .get_host_object(isa)
            .and_then(|h| h.as_any().downcast_ref::<ClassHostObject>())
            .map(|c| c.instance_size)
            .unwrap_or(guest_size_of::<objc_object>());

        self.alloc_object_inner(
            isa,
            instance_size,
            host_object,
            mem,
            Some(NonZeroU32::new(1).unwrap()),
        )
    }

    /// Like [Self::alloc_object], but with an explicit guest instance size.
    /// Needed for classes whose guest-memory layout carries the object's
    /// state (so guest code that bit-copies the object — e.g. Gameloft's
    /// engines copying a 0x40-byte UITouch — gets a fully functional copy).
    pub fn alloc_object_sized(
        &mut self,
        isa: Class,
        instance_size: GuestUSize,
        host_object: Box<dyn AnyHostObject>,
        mem: &mut Mem,
    ) -> id {
        self.alloc_object_inner(
            isa,
            instance_size,
            host_object,
            mem,
            Some(NonZeroU32::new(1).unwrap()),
        )
    }

    pub fn alloc_static_object(
        &mut self,
        isa: Class,
        host_object: Box<dyn AnyHostObject>,
        mem: &mut Mem,
    ) -> id {
        let size = guest_size_of::<objc_object>();
        self.alloc_object_inner(isa, size, host_object, mem, None)
    }

    pub fn register_static_object(
        &mut self,
        guest_object: id,
        host_object: Box<dyn AnyHostObject>,
    ) {
        if guest_object == nil {
            return;
        }
        self.forget_freed_object(guest_object);
        self.objects.insert(
            guest_object,
            HostObjectEntry {
                host_object,
                refcount: None,
                isa: None,
            },
        );
    }

    pub fn get_host_object(&self, object: id) -> Option<&dyn AnyHostObject> {
        if object == nil {
            return None;
        }
        self.objects.get(&object).map(|entry| &*entry.host_object)
    }

    pub fn borrow<T: AnyHostObject + Default + 'static>(&self, object: id) -> &T {
        if let Some(entry) = self.objects.get(&object) {
            let mut host_object: &(dyn AnyHostObject + 'static) = &*entry.host_object;
            loop {
                if let Some(res) = host_object.as_any().downcast_ref() {
                    return res;
                } else if let Some(next) = host_object.as_superclass() {
                    host_object = next;
                } else {
                    break;
                }
            }
        }

        // Fallback for missing / wrong-type objects.
        //
        // Previously we returned a reference to a single shared
        // `static DUMMY_BUF: [u64; 256] = [0; 256]`. That one buffer was
        // aliased across EVERY fake borrow of EVERY type, so as soon as a
        // `borrow_mut` populated e.g. `UIViewHostObject.subviews` with a
        // non-empty Vec, every subsequent fake borrow saw the same list —
        // including of itself, causing `hitTest:` to recurse infinitely and
        // overflow the host stack.
        //
        // We now leak a fresh zero-initialized buffer per (id, type) pair
        // so the returned reference has stable, isolated storage. A proper
        // fix would register a real `Default::default()` host object, but
        // that requires a `T: Default` bound which many callers don't yet
        // provide.
        // POSIX/Objective-C semantics: a message to `nil` returns the
        // zero/empty form of the return type — the runtime is expected
        // to treat such calls as a no-op. Returning a zero-initialized
        // phantom host object preserves this without flooding the log.
        if object == nil {
            log_dbg!(
                "borrow on nil receiver of type {} — returning zero-initialized phantom",
                std::any::type_name::<T>()
            );
        } else if let Some(entry) = self.objects.get(&object) {
            // The object exists but its host object is a different type than
            // requested. Reporting the actual type makes these mismatches
            // diagnosable — it's usually either a guest pointer/type confusion
            // or a host class that forgot to embed its superclass host object
            // (see `impl_HostObject_with_superclass!`).
            if self.warn_fake_borrow_once(object, std::any::TypeId::of::<T>()) {
                log!(
                    "Warning: SUPER HACK! Faking borrow for wrong-type object {:?}: \
                     requested {}, actual host object {} (suppressing further warnings \
                     for this object/type pair)",
                    object,
                    std::any::type_name::<T>(),
                    host_object_description(&*entry.host_object),
                );
            }
        } else if self.warn_fake_borrow_once(object, std::any::TypeId::of::<T>()) {
            log!(
                "Warning: SUPER HACK! Faking borrow for missing object {:?} of type {} \
                 (suppressing further warnings for this object/type pair)",
                object,
                std::any::type_name::<T>()
            );
        }
        phantom_host_object::<T>(object)
    }

    pub fn borrow_mut<T: AnyHostObject + Default + 'static>(&mut self, object: id) -> &mut T {
        if let Some(entry) = self.objects.get_mut(&object) {
            type Aho = dyn AnyHostObject + 'static;
            let mut host_object: &mut Aho = &mut *entry.host_object;
            loop {
                let current_ptr = host_object as *mut Aho;
                if let Some(res) = unsafe { &mut *current_ptr }.as_any_mut().downcast_mut() {
                    return res;
                }

                let has_super = unsafe { &*current_ptr }.as_superclass().is_some();
                if has_super {
                    host_object = unsafe { &mut *current_ptr }.as_superclass_mut().unwrap();
                } else {
                    break;
                }
            }
        }

        // See comment in `borrow` above for rationale.
        if object == nil {
            log_dbg!(
                "borrow_mut on nil receiver of type {} — returning zero-initialized phantom",
                std::any::type_name::<T>()
            );
        } else if self.warn_fake_borrow_once(object, std::any::TypeId::of::<T>()) {
            log!(
                "Warning: SUPER HACK! Faking borrow_mut for missing object {:?} of type {} \
                 (suppressing further warnings for this object/type pair)",
                object,
                std::any::type_name::<T>()
            );
        }
        phantom_host_object_mut::<T>(object)
    }

    /// Returns `true` the first time a fake borrow is attempted for the
    /// (object, host type) pair, `false` afterwards. Keeps the diagnostic
    /// value of the SUPER HACK warnings while preventing per-frame spam.
    fn warn_fake_borrow_once(&self, object: id, ty: std::any::TypeId) -> bool {
        self.fake_borrow_warned
            .lock()
            .unwrap()
            .insert((object, ty))
    }

    pub fn get_refcount(&mut self, object: id) -> NonZeroU32 {
        let default_rc = NonZeroU32::new(1).unwrap();
        if object == nil {
            return default_rc;
        }

        self.objects
            .get(&object)
            .and_then(|e| e.refcount)
            .unwrap_or(default_rc)
    }

    pub fn increment_refcount(&mut self, object: id) {
        if object == nil {
            return;
        }
        if let Some(entry) = self.objects.get_mut(&object) {
            if let Some(refcount) = entry.refcount.as_mut() {
                if let Some(new_rc) = refcount.get().checked_add(1) {
                    *refcount = NonZeroU32::new(new_rc).unwrap();
                }
            }
        }
    }

    #[must_use]
    pub fn decrement_refcount(&mut self, object: id) -> bool {
        if object == nil {
            return false;
        }
        if let Some(entry) = self.objects.get_mut(&object) {
            if let Some(refcount) = entry.refcount.as_mut() {
                if refcount.get() == 1 {
                    entry.refcount = None;
                    return true;
                } else {
                    *refcount = NonZeroU32::new(refcount.get() - 1).unwrap();
                }
            }
        }
        false
    }

    /// Look up the instance size (in bytes, including the `isa` pointer) for
    /// a class. Falls back to the size of a bare `objc_object` (just the isa)
    /// when the class has no registered [ClassHostObject] — matching the
    /// minimum size the runtime would allocate.
    pub fn class_instance_size(&self, class: Class) -> GuestUSize {
        self.get_host_object(class)
            .and_then(|h| h.as_any().downcast_ref::<ClassHostObject>())
            .map(|c| c.instance_size)
            .unwrap_or(guest_size_of::<objc_object>())
    }

    /// The runtime primitive behind Apple's (deprecated) `NSCopyObject`:
    /// create an exact, shallow, byte-for-byte copy of `object`.
    ///
    /// Per Apple's documentation, `NSCopyObject` "Creates an exact copy of an
    /// object." It allocates a new instance of the same class as `object`
    /// (plus `extra_bytes` of trailing storage) and copies the bytes of the
    /// original instance into it. This is a *shallow* copy: object-pointer
    /// ivars are duplicated as raw pointers without any extra retain, exactly
    /// like the real implementation (classes that adopt `NSCopying` via
    /// `NSCopyObject` are responsible for fixing up retained ivars
    /// themselves). The new object starts with a reference count of 1.
    ///
    /// Returns `nil` if `object` is `nil`.
    pub fn object_copy(&mut self, object: id, extra_bytes: GuestUSize, mem: &mut Mem) -> id {
        if object == nil {
            return nil;
        }
        let isa = Self::read_isa(object, mem);
        let instance_size = self.class_instance_size(isa);
        let total_size = instance_size.saturating_add(extra_bytes);

        let new_object: id = mem.alloc(total_size).cast();
        // Copy the original instance's bytes verbatim (this includes the isa
        // pointer, all ivars, and — if requested — leaves the trailing
        // `extra_bytes` zero-initialised, which `mem.alloc` guarantees).
        let src_bytes: Vec<u8> = mem.bytes_at(object.cast(), instance_size).to_vec();
        mem.bytes_at_mut(new_object.cast(), instance_size)
            .copy_from_slice(&src_bytes);

        // Register a host-side record so the copy is a first-class object
        // with its own reference count. A bitwise NSCopyObject copy does not
        // duplicate host-side state, so we attach a TrivialHostObject — this
        // matches the real runtime, where NSCopyObject is only used by classes
        // that keep all of their state in guest-memory ivars.
        self.forget_freed_object(new_object);
        self.objects.insert(
            new_object,
            HostObjectEntry {
                host_object: Box::new(TrivialHostObject),
                refcount: Some(NonZeroU32::new(1).unwrap()),
                isa: Some(isa),
            },
        );
        new_object
    }

    /// Tear down an object: run the runtime-side bookkeeping that `-dealloc`
    /// implementations rely on, then free the guest allocation.
    ///
    /// This is deliberately defensive about being called twice or on a pointer
    /// the runtime never handed out (a corrupted `self`, a guest that releases
    /// an object one time too many, or our own recovery paths). The previous
    /// implementation silently ignored such calls, which hid the double-free
    /// and left the log with no clue about which object was involved. Freeing
    /// again would be far worse than ignoring the call, because the address may
    /// already belong to an unrelated live allocation, so we keep the
    /// "no-op" behaviour and make it loud (once per address).
    pub fn dealloc_object(&mut self, object: id, mem: &mut Mem) {
        if object == nil {
            return;
        }

        // ARC weak-reference contract: zero out every `__weak` slot
        // that referred to this object before its memory is freed, so
        // subsequent `objc_loadWeakRetained` calls correctly observe
        // `nil`. This must happen before we drop the host object,
        // because the writeback uses guest memory only.
        self.zero_weak_references_for(object, mem);

        let Some(entry) = self.objects.remove(&object) else {
            // Double `-dealloc`, or `-dealloc` on an object the runtime does
            // not know. Report it (throttled per address) and do NOT touch
            // guest memory: the address may belong to another live allocation
            // by now. Already reported when it was first freed? Stay quiet, so
            // a broken `-dealloc` that loops cannot flood the log.
            let already_reported = self
                .freed_objects
                .get(&object)
                .map(|record| record.double_free_reported)
                .unwrap_or(false);
            if !already_reported {
                match self
                    .freed_objects
                    .get(&object)
                    .and_then(|record| record.class_name.clone())
                {
                    Some(name) => log!(
                        "Warning: dealloc_object({object:?}) — the object was already \
                         deallocated (it was a \"{name}\"). Ignoring the second \
                         deallocation instead of freeing a possibly reused address. \
                         This is a guest over-release bug."
                    ),
                    None => log!(
                        "Warning: dealloc_object({object:?}) — the runtime has no record \
                         of this pointer being allocated. Ignoring instead of freeing \
                         memory that may belong to another allocation. This indicates \
                         a corrupted `self` or an unbalanced release."
                    ),
                }
                if let Some(record) = self.freed_objects.get_mut(&object) {
                    record.double_free_reported = true;
                }
            }
            return;
        };

        // Remember what died *before* the host object (and its name) is
        // dropped, so a later message to this address can be reported as a
        // use-after-free with the real class name.
        let class = entry.isa;
        let class_name = class.and_then(|class| self.try_get_class_name(class).map(|s| s.to_owned()));

        std::mem::drop(entry.host_object);
        mem.free(object.cast());
        self.record_freed_object(object, class, class_name);
    }

    /// Remember that `object` was deallocated, keeping at most
    /// [`FREED_OBJECT_MEMORY`] records (oldest evicted first).
    fn record_freed_object(&mut self, object: id, class: Option<Class>, class_name: Option<String>) {
        if self
            .freed_objects
            .insert(
                object,
                FreedObjectRecord {
                    class,
                    class_name,
                    double_free_reported: false,
                },
            )
            .is_none()
        {
            self.freed_object_order.push_back(object);
        }
        while self.freed_object_order.len() > FREED_OBJECT_MEMORY {
            if let Some(oldest) = self.freed_object_order.pop_front() {
                self.freed_objects.remove(&oldest);
            }
        }
    }

    /// Drop the freed-object record for a recycled address.
    fn forget_freed_object(&mut self, object: id) {
        if self.freed_objects.remove(&object).is_some() {
            // The order deque is only a size bound, so leave stale entries in
            // it; drop them from the front as they surface. (`front()`'s borrow
            // is released by `copied()` before the deque is mutated.)
            loop {
                let Some(front) = self.freed_object_order.front().copied() else {
                    break;
                };
                if self.freed_objects.contains_key(&front) {
                    break;
                }
                self.freed_object_order.pop_front();
            }
        }
    }

    /// Explain why `object` has a `nil` `isa`, without touching guest memory.
    ///
    /// The dispatcher uses this to distinguish a plain use-after-free (the
    /// object is gone; return `nil` and let the guest cope) from a clobbered
    /// `isa` on a live object, which the runtime can repair because it kept the
    /// authoritative class host-side.
    pub(crate) fn diagnose_nil_isa(&self, object: id) -> NilIsaDiagnosis {
        if let Some(record) = self.freed_objects.get(&object) {
            return NilIsaDiagnosis::Freed {
                class: record.class,
                class_name: record.class_name.clone(),
            };
        }
        // A root class object legitimately has a nil `isa` (see
        // `link_class_inner`'s NSObject special case), so only a *non-nil*
        // recorded class is evidence that the slot was clobbered.
        if let Some(isa) = self
            .objects
            .get(&object)
            .and_then(|entry| entry.isa)
            .filter(|isa| *isa != nil)
        {
            return NilIsaDiagnosis::Clobbered { class: isa };
        }
        NilIsaDiagnosis::Unknown
    }

    /// Restore the `isa` of a live, registered object whose `isa` slot was
    /// overwritten with `nil`/garbage by non-ObjC code.
    ///
    /// Returns `None` (and writes nothing) if the runtime does not know the
    /// object's class, in which case the caller must treat the receiver as dead
    /// rather than guessing.
    pub(crate) fn repair_isa(&self, object: id, mem: &mut Mem) -> Option<Class> {
        let class = self.objects.get(&object).and_then(|entry| entry.isa)?;
        mem.write(object, objc_object { isa: class });
        Some(class)
    }

    /// The class an object was allocated as, if the runtime allocated it.
    pub(crate) fn registered_isa(&self, object: id) -> Option<Class> {
        self.objects.get(&object).and_then(|entry| entry.isa)
    }

    /// Convert an `UnimplementedClass` placeholder into a real
    /// [ClassHostObject] in place.
    ///
    /// Runtime mutators (`class_addMethod`, `class_replaceMethod`,
    /// `class_setSuperclass`, `method_exchangeImplementations`, …) and
    /// category installation used to call `borrow_mut::<ClassHostObject>`
    /// unconditionally. When the target was a class we had only stubbed (an
    /// `UnimplementedClass` placeholder), that borrow produced a *phantom* host
    /// object: the mutation was written into a leaked scratch buffer and
    /// silently thrown away, so the method never actually existed while the log
    /// filled with "SUPER HACK! Faking borrow_mut" warnings. Promoting the
    /// placeholder keeps the class's name and metaclass-ness and makes the
    /// mutation observable, which is what the guest expects. (`FakeClass` is
    /// deliberately *not* promoted — see below.)
    pub(crate) fn promote_placeholder_class(&mut self, class: Class) -> bool {
        let Some(entry) = self.objects.get_mut(&class) else {
            return false;
        };
        // Only `UnimplementedClass` placeholders are promoted. A `FakeClass`
        // deliberately stands in for a defunct third-party SDK whose code we do
        // not want to run at all (see `substitute_classes`), so a mutation on
        // one must stay a no-op rather than revive the SDK; the callers log
        // that the mutation was refused.
        let (name, is_metaclass) = match entry
            .host_object
            .as_any()
            .downcast_ref::<UnimplementedClass>()
        {
            Some(unimplemented) => (unimplemented.name.clone(), unimplemented.is_metaclass),
            None => return false,
        };
        let instance_size = guest_size_of::<objc_object>();
        entry.host_object = Box::new(ClassHostObject {
            name,
            is_metaclass,
            superclass: nil,
            instance_start: instance_size,
            instance_size,
            ..Default::default()
        });
        true
    }

    /// `&mut ClassHostObject` for a class, promoting a placeholder first.
    ///
    /// Unlike `borrow_mut::<ClassHostObject>` this never falls back to a
    /// phantom, so callers can rely on their mutation sticking.
    pub(crate) fn class_host_object_mut(&mut self, class: Class) -> Option<&mut ClassHostObject> {
        if !self.is_known_class_host_object(class) {
            self.promote_placeholder_class(class);
        }
        let entry = self.objects.get_mut(&class)?;
        type Aho = dyn AnyHostObject + 'static;
        let mut host_object: &mut Aho = &mut *entry.host_object;
        // Same superclass-chain walk as `borrow_mut`, but strictly typed: this
        // only ever yields a real `ClassHostObject`.
        loop {
            let current_ptr = host_object as *mut Aho;
            // SAFETY: `current_ptr` points into the boxed host object owned by
            // `self.objects`, which is not moved or aliased while this borrow
            // lives; each step re-borrows the same allocation, matching the
            // existing `borrow_mut` implementation.
            if let Some(res) = unsafe { &mut *current_ptr }.as_any_mut().downcast_mut() {
                return Some(res);
            }
            let has_superclass = unsafe { &*current_ptr }.as_superclass().is_some();
            if has_superclass {
                host_object = unsafe { &mut *current_ptr }.as_superclass_mut().unwrap();
            } else {
                return None;
            }
        }
    }

    /// Immutable, strict counterpart of [Self::class_host_object_mut].
    ///
    /// Returns `None` for classes that are only placeholders instead of
    /// allocating a phantom `ClassHostObject` on the fly. A phantom has an
    /// empty name and a `nil` superclass, so answering runtime metadata
    /// queries from one silently invented a root class — and produced the
    /// "Faking borrow ... requested ClassHostObject, actual host type
    /// UnimplementedClass" warnings that hid the real question (which class
    /// was being asked about).
    pub(crate) fn class_host_object(&self, class: Class) -> Option<&ClassHostObject> {
        let entry = self.objects.get(&class)?;
        let mut host_object: &(dyn AnyHostObject + 'static) = &*entry.host_object;
        loop {
            if let Some(class_object) = host_object.as_any().downcast_ref::<ClassHostObject>() {
                return Some(class_object);
            }
            host_object = host_object.as_superclass()?;
        }
    }

    /// Name and metaclass flag of any linked class, including placeholders.
    ///
    /// Apple's runtime exposes these two facts for every class object it knows
    /// about, so the emulator should too: an `UnimplementedClass` still has a
    /// name (`class_getName`) and is still a class or a metaclass
    /// (`class_isMetaClass`). Only the method table and ivar layout are
    /// genuinely unknown for a placeholder.
    pub(crate) fn class_like_metadata(&self, class: Class) -> Option<(&str, bool)> {
        let host_object = self.get_host_object(class)?;
        let any = host_object.as_any();
        if let Some(class_object) = any.downcast_ref::<ClassHostObject>() {
            Some((class_object.name.as_str(), class_object.is_metaclass))
        } else if let Some(unimplemented) = any.downcast_ref::<UnimplementedClass>() {
            Some((unimplemented.name.as_str(), unimplemented.is_metaclass))
        } else if let Some(fake) = any.downcast_ref::<FakeClass>() {
            Some((fake.name.as_str(), fake.is_metaclass))
        } else {
            None
        }
    }

    fn is_known_class_host_object(&self, class: Class) -> bool {
        self.get_host_object(class)
            .and_then(|host_object| host_object.as_any().downcast_ref::<ClassHostObject>())
            .is_some()
    }
}

/// Diagnostics record for an object the runtime has freed.
pub(super) struct FreedObjectRecord {
    /// Class the object had when it was deallocated, if known.
    pub(super) class: Option<Class>,
    /// Name of that class, resolved at deallocation time so it stays readable
    /// even if the class is later unloaded.
    pub(super) class_name: Option<String>,
    /// Whether the second-`dealloc` warning was already emitted for this
    /// address, to keep a looping `-dealloc` from flooding the log.
    pub(super) double_free_reported: bool,
}

/// Why a receiver's `isa` reads back as `nil`.
pub(crate) enum NilIsaDiagnosis {
    /// The address was deallocated by the runtime: messaging it is a
    /// use-after-free in the guest app.
    Freed {
        class: Option<Class>,
        class_name: Option<String>,
    },
    /// The object is still alive in the runtime's table, so the `isa` slot was
    /// clobbered. The authoritative class is known and the slot can be
    /// repaired.
    Clobbered { class: Class },
    /// Neither: a pointer the runtime never allocated, or a freed address that
    /// has already aged out of the diagnostics ring.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objc::ObjC;

    /// Minimal runtime: an `ObjC` with no registered host classes other than
    /// the ones the test creates, and a `Mem` with a null segment.
    fn runtime() -> (ObjC, Mem) {
        let mut mem = Mem::new();
        mem.set_null_segment_size(0x1000);
        (ObjC::new(), mem)
    }

    #[test]
    fn nil_isa_is_repaired_for_live_objects_and_reported_as_freed_after_dealloc() {
        let (mut objc, mut mem) = runtime();
        let class = objc.alloc_static_object(
            nil,
            Box::new(ClassHostObject {
                name: "Widget".to_string(),
                instance_size: 8,
                ..Default::default()
            }),
            &mut mem,
        );
        let object = objc.alloc_object(class, Box::new(TrivialHostObject), &mut mem);

        // A live object whose isa slot was zeroed can be repaired, because the
        // runtime recorded the class it was allocated as.
        mem.write(object, objc_object { isa: nil });
        match objc.diagnose_nil_isa(object) {
            NilIsaDiagnosis::Clobbered { class: found } => assert!(found == class),
            _ => panic!("a live object with a zeroed isa must be diagnosed as clobbered"),
        }
        assert!(objc.repair_isa(object, &mut mem) == Some(class));
        assert!(ObjC::read_isa(object, &mem) == class);

        // After deallocation the same address is a use-after-free, and the
        // report still names the class it had.
        objc.dealloc_object(object, &mut mem);
        match objc.diagnose_nil_isa(object) {
            NilIsaDiagnosis::Freed {
                class: found,
                class_name,
            } => {
                assert!(found == Some(class));
                assert_eq!(class_name.as_deref(), Some("Widget"));
            }
            _ => panic!("a deallocated object must be diagnosed as freed"),
        }
    }

    #[test]
    fn recycled_addresses_forget_their_freed_object_record() {
        let (mut objc, mut mem) = runtime();
        let class = objc.alloc_static_object(
            nil,
            Box::new(ClassHostObject {
                name: "Widget".to_string(),
                instance_size: 8,
                ..Default::default()
            }),
            &mut mem,
        );
        let object = objc.alloc_object(class, Box::new(TrivialHostObject), &mut mem);
        objc.dealloc_object(object, &mut mem);
        assert!(matches!(
            objc.diagnose_nil_isa(object),
            NilIsaDiagnosis::Freed { .. }
        ));

        // The allocator may hand the same address out again; the new object is
        // alive, so it must never be reported as the dead one (which would send
        // a user-after-free hunt after an innocent class).
        let reused = objc.alloc_object(class, Box::new(TrivialHostObject), &mut mem);
        if reused == object {
            assert!(matches!(
                objc.diagnose_nil_isa(reused),
                NilIsaDiagnosis::Clobbered { .. } | NilIsaDiagnosis::Unknown
            ));
        }
    }

    #[test]
    fn double_dealloc_is_ignored_and_does_not_touch_freed_memory() {
        let (mut objc, mut mem) = runtime();
        let class = objc.alloc_static_object(
            nil,
            Box::new(ClassHostObject {
                name: "Widget".to_string(),
                instance_size: 8,
                ..Default::default()
            }),
            &mut mem,
        );
        let object = objc.alloc_object(class, Box::new(TrivialHostObject), &mut mem);
        objc.dealloc_object(object, &mut mem);
        // Must not panic, and must not free the block a second time.
        objc.dealloc_object(object, &mut mem);
        assert!(matches!(
            objc.diagnose_nil_isa(object),
            NilIsaDiagnosis::Freed { .. }
        ));
        // A pointer the runtime never allocated is also a no-op.
        let unallocated: id = Ptr::from_bits(0x4000);
        objc.dealloc_object(unallocated, &mut mem);
    }

    #[test]
    fn nil_isa_diagnosis_ignores_a_nil_recorded_class() {
        let (mut objc, mut mem) = runtime();
        // A root class object is registered with a nil isa; messaging it must
        // not be "repaired" into a class it never had.
        let root = objc.alloc_static_object(
            nil,
            Box::new(ClassHostObject {
                name: "Root".to_string(),
                instance_size: 8,
                ..Default::default()
            }),
            &mut mem,
        );
        assert!(matches!(
            objc.diagnose_nil_isa(root),
            NilIsaDiagnosis::Unknown
        ));
        assert!(objc.repair_isa(root, &mut mem).is_none());
    }
}

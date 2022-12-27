//! Handling of Objective-C objects.
//!
//! Note that classes and metaclasses are objects too!
//!
//! Resources:
//! - [Apple's documentation of `id`](https://developer.apple.com/documentation/objectivec/id)
//!   (which for some reason omits that `id` is a pointer type)
//!
//! To make things easier for the host code, our implementation will maintain
//! two linked representations of an object: an [objc_object] struct allocated
//! in guest memory, which needs to maintain the same ABI that Apple's runtime
//! does, and a [HostObject] trait object allocated in host memory, which can be
//! used for any data that only our host code needs to access. As a bonus we get
//! some resilience against guest memory corruption.
//!
//! See also: [crate::frameworks::foundation::ns_object].

use super::Class;
use crate::mem::{Mem, MutPtr, Ptr, SafeRead};
use std::any::Any;
use std::num::NonZeroU32;

/// Memory layout of a minimal Objective-C object. See [id].
///
/// The name comes from `objc_object` in Apple's runtime.
#[repr(C, packed)]
pub struct objc_object {
    /// In life, sometimes we must ask ourselves... what is existence?
    /// What is the meaning in love and suffering? What is it that drives us to
    /// know? What is the joy in longing for absolutes in a universe abundant
    /// in beautiful subjectivity?
    ///
    /// The `isa` pointer cannot answer these questions.
    ///
    /// But it does tell you what class an object belongs to.
    isa: Class,
}
impl SafeRead for objc_object {}

/// Generic pointer to an Objective-C object (including classes or metaclasses).
///
/// The name is standard Objective-C.
#[allow(non_camel_case_types)]
pub type id = MutPtr<objc_object>;

/// Null pointer for Objective-C objects.
///
/// The name is standard Objective-C.
#[allow(non_upper_case_globals)]
pub const nil: id = Ptr::null();

/// Struct used to track the host object and refcount of every object.
/// Maybe debugging info too eventually?
///
/// If the `refcount` is `None`, that means this object has a static duration
/// and should not be reference-counted, e.g. it is a class.
pub(super) struct HostObjectEntry {
    host_object: Box<dyn AnyHostObject>,
    refcount: Option<NonZeroU32>,
}

/// Type for host objects.
pub trait HostObject: Any + 'static {}

/// Trait wrapping [HostObject] with a blanket implementation to make
/// downcasting work. Don't implement it yourself.
///
/// This is a workaround for it not being possible to directly cast
/// `&'a dyn HostObject` to `&'a dyn Any`.
pub trait AnyHostObject {
    fn as_any<'a>(&'a self) -> &'a (dyn Any + 'static);
    fn as_any_mut<'a>(&'a mut self) -> &'a mut (dyn Any + 'static);
}
impl<T: HostObject> AnyHostObject for T {
    fn as_any<'a>(&'a self) -> &'a (dyn Any + 'static) {
        self
    }
    fn as_any_mut<'a>(&'a mut self) -> &'a mut (dyn Any + 'static) {
        self
    }
}

/// Empty host object used by `[NSObject alloc]`.
pub struct TrivialHostObject;
impl HostObject for TrivialHostObject {}

impl super::ObjC {
    /// Read the all-important `isa`.
    pub(super) fn read_isa(object: id, mem: &Mem) -> Class {
        mem.read(object).isa
    }
    /// Write the all-important `isa`.
    pub(super) fn write_isa(object: id, isa: Class, mem: &mut Mem) {
        mem.write(object.cast(), isa)
    }

    fn alloc_object_inner(
        &mut self,
        isa: Class,
        host_object: Box<dyn AnyHostObject>,
        mem: &mut Mem,
        refcount: Option<NonZeroU32>,
    ) -> id {
        let guest_object = objc_object { isa };
        let ptr = mem.alloc_and_write(guest_object);
        assert!(!self.objects.contains_key(&ptr));
        self.objects.insert(
            ptr,
            HostObjectEntry {
                host_object,
                refcount,
            },
        );
        ptr
    }

    /// Allocate a reference-counted (guest) object (like `[NSObject alloc]`)
    /// and associate it with its host object.
    pub fn alloc_object(
        &mut self,
        isa: Class,
        host_object: Box<dyn AnyHostObject>,
        mem: &mut Mem,
    ) -> id {
        self.alloc_object_inner(isa, host_object, mem, Some(NonZeroU32::new(1).unwrap()))
    }

    /// Allocate a static-lifetime (guest) object (for example, a class) and
    /// associate it with its host object.
    pub(super) fn alloc_static_object(
        &mut self,
        isa: Class,
        host_object: Box<dyn AnyHostObject>,
        mem: &mut Mem,
    ) -> id {
        self.alloc_object_inner(isa, host_object, mem, None)
    }

    /// Associate a host object with an existing static-lifetime (guest) object
    /// (for example, a class).
    pub(super) fn register_static_object(
        &mut self,
        guest_object: id,
        host_object: Box<dyn AnyHostObject>,
    ) {
        assert!(!self.objects.contains_key(&guest_object));
        self.objects.insert(
            guest_object,
            HostObjectEntry {
                host_object,
                refcount: None,
            },
        );
    }

    /// Get a reference to a host object, if the object exists.
    pub(super) fn get_host_object(&self, object: id) -> Option<&dyn AnyHostObject> {
        self.objects.get(&object).map(|entry| &*entry.host_object)
    }

    #[allow(dead_code)]
    /// Get a reference to a host object and downcast it. Panics if there is
    /// no such object, or if downcasting fails.
    pub fn borrow<T: AnyHostObject + 'static>(&self, object: id) -> &T {
        let entry = self.objects.get(&object).unwrap();
        entry.host_object.as_any().downcast_ref().unwrap()
    }

    /// Get a reference to a host object and downcast it. Panics if there is
    /// no such object, or if downcasting fails.
    pub fn borrow_mut<T: AnyHostObject + 'static>(&mut self, object: id) -> &mut T {
        let entry = self.objects.get_mut(&object).unwrap();
        entry.host_object.as_any_mut().downcast_mut().unwrap()
    }

    /// Increase the refcount of a reference-counted object. Do not call this
    /// directly unless you're implementing `release` on `NSObject`. That method
    /// may be overridden.
    pub fn increment_refcount(&mut self, object: id) {
        let Some(entry) = self.objects.get_mut(&object) else {
            panic!("No entry found for object {:?}, it may have already been deallocated", object);
        };
        let Some(refcount) = entry.refcount.as_mut() else {
            // Might mean a missing `retain` override.
            panic!("Attempt to increment refcount on static-lifetime object {:?}!", object);
        };
        *refcount = refcount.checked_add(1).unwrap();
    }

    /// Decrease the refcount of a reference-counted object. Do not call this
    /// directly unless you're implementing `release` on `NSObject`. That method
    /// may be overridden.
    ///
    /// If the return value is `true`, the object needs to be deallocated. Send
    /// it the `dealloc` message.
    #[must_use]
    pub fn decrement_refcount(&mut self, object: id) -> bool {
        let Some(entry) = self.objects.get_mut(&object) else {
            panic!("No entry found for object {:?}, it may have already been deallocated", object);
        };
        let Some(refcount) = entry.refcount.as_mut() else {
            // Might mean a missing `release` override.
            panic!("Attempt to decrement refcount on static-lifetime object {:?}!", object);
        };
        if refcount.get() == 1 {
            entry.refcount = None;
            true
        } else {
            *refcount = NonZeroU32::new(refcount.get() - 1).unwrap();
            false
        }
    }

    /// Deallocate an object. Do not call this directly unless you're
    /// implementing `dealloc` on `NSObject`.
    pub fn dealloc_object(&mut self, object: id, mem: &mut Mem) {
        let HostObjectEntry {
            host_object,
            refcount,
        } = self.objects.remove(&object).unwrap();
        assert!(refcount.is_none());
        std::mem::drop(host_object);

        mem.free(object.cast());
    }
}
/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `NSPersistentStoreCoordinator` and related Core Data classes.
//! The managed object model, entity registry and object graph are implemented
//! (apps routinely build their model in code and ask the context for entities
//! during start-up); persistence to SQLite is not, and store files are never
//! touched.

use crate::dyld::{ConstantExports, HostConstant};
use crate::libc::dispatch::call_void_block;
use crate::frameworks::foundation::ns_string;
use crate::frameworks::foundation::{NSInteger, NSUInteger};
use crate::mem::MutPtr;
use crate::objc::{
    autorelease, id, impl_HostObject_with_superclass, msg, msg_class, nil, objc_classes, release,
    retain, Class, ClassExports, HostObject, NSZonePtr,
};

// MARK: - NSPersistentStoreCoordinator

#[derive(Default)]
struct NSPersistentStoreCoordinatorHostObject {
    /// `NSManagedObjectModel*`
    managed_object_model: id,
    /// `NSArray<NSPersistentStore*>*`
    persistent_stores: id,
}
impl HostObject for NSPersistentStoreCoordinatorHostObject {}

// MARK: - NSPersistentStore

#[derive(Default)]
struct NSPersistentStoreHostObject {
    /// `NSString*`
    type_: id,
    /// `NSURL*`
    url: id,
    /// `NSDictionary*`
    options: id,
    /// `NSPersistentStoreCoordinator*` — weak
    coordinator: id,
}
impl HostObject for NSPersistentStoreHostObject {}

// MARK: - NSManagedObjectModel

#[derive(Default)]
struct NSManagedObjectModelHostObject {
    /// `NSArray<NSEntityDescription*>*`
    entities: id,
}
impl HostObject for NSManagedObjectModelHostObject {}

// MARK: - NSManagedObjectContext

#[derive(Default)]
struct NSManagedObjectContextHostObject {
    /// `NSPersistentStoreCoordinator*`
    persistent_store_coordinator: id,
    /// `NSManagedObjectContext*` — parent context (weak)
    parent_context: id,
    undo_manager: id,
    merge_policy: id,
    /// `NSMutableSet<NSManagedObject*>*` — every object registered with this
    /// context, inserted or fetched.
    registered_objects: id,
    /// `NSMutableSet<NSManagedObject*>*` — registered objects that were
    /// inserted and not yet saved.
    inserted_objects: id,
    /// `NSMutableSet<NSManagedObject*>*` — objects deleted since the last save.
    deleted_objects: id,
}
impl HostObject for NSManagedObjectContextHostObject {}

/// For failure paths only: on success, Cocoa leaves `*error` alone and
/// callers may pass an uninitialized pointer.
fn clear_error_out_parameter(env: &mut crate::Environment, error: MutPtr<id>) {
    if !error.is_null() {
        env.mem.write(error, nil);
    }
}

// MARK: - Model lookups

/// Compares two `NSString`s through the guest's own string class, so guest
/// string subclasses (constant strings, `NSCFString`, …) behave correctly and
/// we never borrow a `StringHostObject` out of an object that isn't one.
fn strings_equal(env: &mut crate::Environment, a: id, b: id) -> bool {
    if a == b {
        return true;
    }
    if a == nil || b == nil {
        return false;
    }
    msg![env; a isEqualToString:b]
}

/// Finds the entity called `name` in `model` (`NSManagedObjectModel`), or
/// `nil`. Per Apple, `-[NSManagedObjectModel entitiesByName]` is the lookup
/// index, but the entities array is the storage of record, so match on it.
fn entity_in_model(env: &mut crate::Environment, model: id, name: id) -> id {
    if model == nil || name == nil {
        return nil;
    }
    let entities: id = msg![env; model entities];
    if entities == nil {
        return nil;
    }
    let count: NSUInteger = msg![env; entities count];
    for i in 0..count {
        let entity: id = msg![env; entities objectAtIndex:i];
        if entity == nil {
            continue;
        }
        let entity_name: id = msg![env; entity name];
        if strings_equal(env, entity_name, name) {
            return entity;
        }
    }
    nil
}

/// `+[NSEntityDescription entityForName:inManagedObjectContext:]`: Apple, _Core
/// Data Programming Guide_: "the entity is looked up in the managed object
/// model associated with the context". That model is reached through the
/// context's persistent store coordinator.
fn entity_named(env: &mut crate::Environment, name: id, context: id) -> id {
    if name == nil || context == nil {
        return nil;
    }
    let coordinator: id = msg![env; context persistentStoreCoordinator];
    if coordinator == nil {
        log_entity_lookup_failure(env, name, "the context has no persistent store coordinator");
        return nil;
    }
    let model: id = msg![env; coordinator managedObjectModel];
    if model == nil {
        log_entity_lookup_failure(env, name, "the coordinator has no managed object model");
        return nil;
    }
    let entity = entity_in_model(env, model, name);
    if entity == nil {
        // A silent `nil` here is what made the original boot hang
        // undiagnosable: the app stops right after this call and says nothing
        // more, so the log has to say why the lookup failed.
        let entities: id = msg![env; model entities];
        let count: NSUInteger = msg![env; entities count];
        let why = format!("the model has {count} entities");
        log_entity_lookup_failure(env, name, &why);
    }
    entity
}

/// Reports a failed `+entityForName:inManagedObjectContext:` lookup by name.
fn log_entity_lookup_failure(env: &mut crate::Environment, name: id, why: &str) {
    let requested = if name == nil {
        "<nil>".to_string()
    } else {
        ns_string::to_rust_string(env, name).into_owned()
    };
    log!("NSEntityDescription: entity \"{requested}\" not found: {why}");
}

/// Indexes a property collection (array or dictionary, as accepted by
/// `-setProperties:`) by property name into `by_name`.
fn index_properties_by_name(env: &mut crate::Environment, properties: id, by_name: id) {
    if properties == nil || by_name == nil {
        return;
    }
    let enumerator: id = msg![env; properties objectEnumerator];
    if enumerator == nil {
        return;
    }
    loop {
        let property: id = msg![env; enumerator nextObject];
        if property == nil {
            break;
        }
        let name: id = msg![env; property name];
        if name != nil {
            () = msg![env; by_name setObject:property forKey:name];
        }
    }
}

/// Returns a new `NSDictionary` of the properties in `by_name` whose class
/// descends from `class_name` (`NSAttributeDescription`, …). Core Data's
/// `-attributesByName` and friends are defined this way.
fn properties_of_class(
    env: &mut crate::Environment,
    by_name: id,
    class_name: &str,
) -> id {
    let filtered: id = msg_class![env; NSMutableDictionary new];
    if by_name == nil {
        return autorelease(env, filtered);
    }
    let wanted: Class = env.objc.get_known_class(class_name, &mut env.mem);
    let enumerator: id = msg![env; by_name objectEnumerator];
    if enumerator == nil {
        return autorelease(env, filtered);
    }
    loop {
        let property: id = msg![env; enumerator nextObject];
        if property == nil {
            break;
        }
        let matches: bool = msg![env; property isKindOfClass:wanted];
        if matches {
            let name: id = msg![env; property name];
            if name != nil {
                () = msg![env; filtered setObject:property forKey:name];
            }
        }
    }
    autorelease(env, filtered)
}

// MARK: - NSManagedObject

#[derive(Default)]
struct NSManagedObjectHostObject {
    /// `NSEntityDescription*`
    entity: id,
    /// `NSManagedObjectContext*` — weak
    managed_object_context: id,
}
impl HostObject for NSManagedObjectHostObject {}

// MARK: - NSEntityDescription

#[derive(Default)]
struct NSEntityDescriptionHostObject {
    /// `NSString*`
    name: id,
    /// `NSString*`
    managed_object_class_name: id,
    /// `NSArray<NSPropertyDescription*>*` or
    /// `NSDictionary<NSString*, NSPropertyDescription*>*`, as passed to
    /// `-setProperties:` (Core Data accepts either).
    properties: id,
    /// `NSDictionary<NSString*, NSPropertyDescription*>*` — `-propertiesByName`.
    properties_by_name: id,
}
impl HostObject for NSEntityDescriptionHostObject {}

// MARK: - NSFetchRequest

#[derive(Default)]
struct NSFetchRequestHostObject {
    /// `NSEntityDescription*`
    entity: id,
    /// `NSString*`
    entity_name: id,
    /// `NSPredicate*`
    predicate: id,
    /// `NSArray<NSSortDescriptor*>*`
    sort_descriptors: id,
    fetch_limit: u64,
    fetch_offset: u64,
    fetch_batch_size: u64,
    returns_objects_as_faults: bool,
    includes_subentities: bool,
}
impl HostObject for NSFetchRequestHostObject {}

// MARK: - NSPropertyDescription

#[derive(Default)]
struct NSPropertyDescriptionHostObject {
    /// `NSString*`
    name: id,
    optional: bool,
    transient: bool,
    /// `NSDictionary*`
    user_info: id,
}
impl HostObject for NSPropertyDescriptionHostObject {}

// MARK: - NSAttributeDescription

#[derive(Default)]
struct NSAttributeDescriptionHostObject {
    /// `NSAttributeDescription` is a subclass of `NSPropertyDescription`, and
    /// inherits its accessors (`-name`, `-setName:`, `-isOptional`, …). Those
    /// accessors borrow `NSPropertyDescriptionHostObject`, so the state has to
    /// be embedded here: without it every `[attribute name]` fell into the
    /// runtime's wrong-type "faking borrow" fallback, which returns a
    /// throw-away zeroed object — i.e. the entity's properties had no names and
    /// could not be looked up.
    superclass: NSPropertyDescriptionHostObject,
    /// NSAttributeType (integer enum)
    attribute_type: NSInteger,
    /// `id` default value
    default_value: id,
    allows_external_binary_data_storage: bool,
}
impl_HostObject_with_superclass!(NSAttributeDescriptionHostObject);

// Apple Core Data — exported `NSString * const` identifiers used as
// store-type tags, error-domain names, store-option keys, notification
// names, and `userInfo` dictionary keys. The literal values must match
// the system's, because apps compare them with `isEqualToString:` and
// `CFStringCompare()` (e.g. `[type isEqualToString:NSSQLiteStoreType]`
// to pick a SQLite store, or `[error.domain
// isEqualToString:NSSQLiteErrorDomain]` to filter SQL errors).
//
// Reference: Apple Core Data Programming Guide,
// `NSPersistentStoreCoordinator.h` and `NSManagedObjectContext.h`.
pub const CONSTANTS: ConstantExports = &[
    // Store-type identifiers
    ("_NSSQLiteStoreType", HostConstant::NSString("SQLite")),
    ("_NSXMLStoreType", HostConstant::NSString("XML")),
    ("_NSBinaryStoreType", HostConstant::NSString("Binary")),
    ("_NSInMemoryStoreType", HostConstant::NSString("InMemory")),
    // Error domain
    (
        "_NSSQLiteErrorDomain",
        HostConstant::NSString("NSSQLiteErrorDomain"),
    ),
    // NSError userInfo keys
    (
        "_NSDetailedErrorsKey",
        HostConstant::NSString("NSDetailedErrorsKey"),
    ),
    (
        "_NSValidationObjectErrorKey",
        HostConstant::NSString("NSValidationObjectErrorKey"),
    ),
    (
        "_NSValidationKeyErrorKey",
        HostConstant::NSString("NSValidationKeyErrorKey"),
    ),
    (
        "_NSValidationPredicateErrorKey",
        HostConstant::NSString("NSValidationPredicateErrorKey"),
    ),
    (
        "_NSValidationValueErrorKey",
        HostConstant::NSString("NSValidationValueErrorKey"),
    ),
    (
        "_NSAffectedStoresErrorKey",
        HostConstant::NSString("NSAffectedStoresErrorKey"),
    ),
    (
        "_NSAffectedObjectsErrorKey",
        HostConstant::NSString("NSAffectedObjectsErrorKey"),
    ),
    (
        "_NSPersistentStoreSaveConflictsErrorKey",
        HostConstant::NSString("NSPersistentStoreSaveConflictsErrorKey"),
    ),
    // Persistent-store option keys
    (
        "_NSReadOnlyPersistentStoreOption",
        HostConstant::NSString("NSReadOnlyPersistentStoreOption"),
    ),
    (
        "_NSPersistentStoreTimeoutOption",
        HostConstant::NSString("NSPersistentStoreTimeoutOption"),
    ),
    (
        "_NSSQLitePragmasOption",
        HostConstant::NSString("NSSQLitePragmasOption"),
    ),
    (
        "_NSSQLiteAnalyzeOption",
        HostConstant::NSString("NSSQLiteAnalyzeOption"),
    ),
    (
        "_NSSQLiteManualVacuumOption",
        HostConstant::NSString("NSSQLiteManualVacuumOption"),
    ),
    (
        "_NSIgnorePersistentStoreVersioningOption",
        HostConstant::NSString("NSIgnorePersistentStoreVersioningOption"),
    ),
    (
        "_NSMigratePersistentStoresAutomaticallyOption",
        HostConstant::NSString("NSMigratePersistentStoresAutomaticallyOption"),
    ),
    (
        "_NSInferMappingModelAutomaticallyOption",
        HostConstant::NSString("NSInferMappingModelAutomaticallyOption"),
    ),
    (
        "_NSStoreUUIDInMetadataKey",
        HostConstant::NSString("NSStoreUUID"),
    ),
    ("_NSStoreTypeKey", HostConstant::NSString("NSStoreType")),
    (
        "_NSStoreModelVersionHashesKey",
        HostConstant::NSString("NSStoreModelVersionHashes"),
    ),
    (
        "_NSStoreModelVersionIdentifiersKey",
        HostConstant::NSString("NSStoreModelVersionIdentifiers"),
    ),
    (
        "_NSPersistentStoreOSCompatibility",
        HostConstant::NSString("NSPersistentStoreOSCompatibility"),
    ),
    // Merge-policy singletons. On real iOS these are `id`
    // (NSMergePolicy*) constants. We expose unique non-NULL slots so
    // identity comparisons (`policy == NSOverwriteMergePolicy`) work.
    // The stub NSManagedObjectContext accepts any non-NULL id as the
    // mergePolicy.
    ("_NSErrorMergePolicy", HostConstant::NullPtr),
    (
        "_NSMergeByPropertyStoreTrumpMergePolicy",
        HostConstant::NullPtr,
    ),
    (
        "_NSMergeByPropertyObjectTrumpMergePolicy",
        HostConstant::NullPtr,
    ),
    ("_NSOverwriteMergePolicy", HostConstant::NullPtr),
    ("_NSRollbackMergePolicy", HostConstant::NullPtr),
    // Notification names
    (
        "_NSManagedObjectContextDidSaveNotification",
        HostConstant::NSString("NSManagedObjectContextDidSaveNotification"),
    ),
    (
        "_NSManagedObjectContextWillSaveNotification",
        HostConstant::NSString("NSManagedObjectContextWillSaveNotification"),
    ),
    (
        "_NSManagedObjectContextObjectsDidChangeNotification",
        HostConstant::NSString("NSManagedObjectContextObjectsDidChangeNotification"),
    ),
    (
        "_NSPersistentStoreCoordinatorStoresDidChangeNotification",
        HostConstant::NSString("NSPersistentStoreCoordinatorStoresDidChangeNotification"),
    ),
    (
        "_NSPersistentStoreCoordinatorStoresWillChangeNotification",
        HostConstant::NSString("NSPersistentStoreCoordinatorStoresWillChangeNotification"),
    ),
    (
        "_NSPersistentStoreCoordinatorWillRemoveStoreNotification",
        HostConstant::NSString("NSPersistentStoreCoordinatorWillRemoveStoreNotification"),
    ),
    (
        "_NSPersistentStoreDidImportUbiquitousContentChangesNotification",
        HostConstant::NSString("NSPersistentStoreDidImportUbiquitousContentChangesNotification"),
    ),
    // Notification userInfo keys
    ("_NSInsertedObjectsKey", HostConstant::NSString("inserted")),
    ("_NSUpdatedObjectsKey", HostConstant::NSString("updated")),
    ("_NSDeletedObjectsKey", HostConstant::NSString("deleted")),
    (
        "_NSRefreshedObjectsKey",
        HostConstant::NSString("refreshed"),
    ),
    (
        "_NSInvalidatedObjectsKey",
        HostConstant::NSString("invalidated"),
    ),
    (
        "_NSInvalidatedAllObjectsKey",
        HostConstant::NSString("invalidatedAll"),
    ),
    (
        "_NSAddedPersistentStoresKey",
        HostConstant::NSString("NSAddedPersistentStoresKey"),
    ),
    (
        "_NSRemovedPersistentStoresKey",
        HostConstant::NSString("NSRemovedPersistentStoresKey"),
    ),
    (
        "_NSUUIDChangedPersistentStoresKey",
        HostConstant::NSString("NSUUIDChangedPersistentStoresKey"),
    ),
    // Ubiquitous (iCloud) Core Data option keys
    (
        "_NSPersistentStoreUbiquitousContentNameKey",
        HostConstant::NSString("NSPersistentStoreUbiquitousContentNameKey"),
    ),
    (
        "_NSPersistentStoreUbiquitousContentURLKey",
        HostConstant::NSString("NSPersistentStoreUbiquitousContentURLKey"),
    ),
    (
        "_NSPersistentStoreUbiquitousPeerTokenOption",
        HostConstant::NSString("NSPersistentStoreUbiquitousPeerTokenOption"),
    ),
    (
        "_NSPersistentStoreRemoveUbiquitousMetadataOption",
        HostConstant::NSString("NSPersistentStoreRemoveUbiquitousMetadataOption"),
    ),
    (
        "_NSPersistentStoreRebuildFromUbiquitousContentOption",
        HostConstant::NSString("NSPersistentStoreRebuildFromUbiquitousContentOption"),
    ),
];

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

// =========================================================================
// NSPersistentStoreCoordinator
// =========================================================================

@implementation NSPersistentStoreCoordinator: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let stores = msg_class![env; NSMutableArray new];
    let host_object = Box::new(NSPersistentStoreCoordinatorHostObject {
        managed_object_model: nil,
        persistent_stores: stores,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

+ (id)metadataForPersistentStoreOfType:(id)_store_type // NSString*
                                   URL:(id)_url        // NSURL*
                                 error:(MutPtr<id>)_error {
    clear_error_out_parameter(env, _error);    // NSError**
    log!("NSPersistentStoreCoordinator metadataForPersistentStoreOfType:URL:error: stubbed");
    nil
}

+ (bool)setMetadata:(id)_metadata               // NSDictionary*
    forPersistentStoreOfType:(id)_store_type    // NSString*
                         URL:(id)_url           // NSURL*
                       error:(MutPtr<id>)_error {
    clear_error_out_parameter(env, _error);       // NSError**
    log!("NSPersistentStoreCoordinator setMetadata:forPersistentStoreOfType:URL:error: stubbed");
    false
}

- (id)initWithManagedObjectModel:(id)model { // NSManagedObjectModel*
    retain(env, model);
    env.objc.borrow_mut::<NSPersistentStoreCoordinatorHostObject>(this)
        .managed_object_model = model;
    this
}

- (())dealloc {
    let host = env.objc.borrow::<NSPersistentStoreCoordinatorHostObject>(this);
    let (model, stores) = (host.managed_object_model, host.persistent_stores);
    release(env, model);
    release(env, stores);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)managedObjectModel {
    env.objc.borrow::<NSPersistentStoreCoordinatorHostObject>(this).managed_object_model
}

- (id)persistentStores {
    env.objc.borrow::<NSPersistentStoreCoordinatorHostObject>(this).persistent_stores
}

- (id)persistentStoreForURL:(id)url { // NSURL*
    let stores = env.objc.borrow::<NSPersistentStoreCoordinatorHostObject>(this).persistent_stores;
    let count: u32 = msg![env; stores count];
    for i in 0..count {
        let store: id = msg![env; stores objectAtIndex:i];
        let store_url: id = msg![env; store URL];
        let eq: bool = msg![env; store_url isEqual:url];
        if eq { return store; }
    }
    nil
}

- (id)URLForPersistentStore:(id)store { // NSPersistentStore*
    msg![env; store URL]
}

- (bool)setURL:(id)_url                   // NSURL*
    forPersistentStore:(id)_store {       // NSPersistentStore*
    log!("NSPersistentStoreCoordinator setURL:forPersistentStore: stubbed -> false");
    false
}

- (id)addPersistentStoreWithType:(id)store_type     // NSString*
                   configuration:(id)_configuration // NSString*
                              URL:(id)url            // NSURL*
                          options:(id)options        // NSDictionary*
                            error:(MutPtr<id>)_error {
    let type_str = if store_type != nil {
        ns_string::to_rust_string(env, store_type).into_owned()
    } else {
        "<nil>".to_string()
    };
    log!(
        "NSPersistentStoreCoordinator addPersistentStoreWithType:{} URL:{:?} — stubbed",
        type_str, url
    );

    // Allocate a stub store object.
    let store: id = msg_class![env; NSPersistentStore alloc];
    let store: id = msg![env; store initWithPersistentStoreCoordinator:this
                                                     configurationName:nil
                                                                   URL:url
                                                               options:options];

    let stores = env.objc.borrow::<NSPersistentStoreCoordinatorHostObject>(this).persistent_stores;
    () = msg![env; stores addObject:store];
    release(env, store);

    // Return the store (retained by the array).
    let stores = env.objc.borrow::<NSPersistentStoreCoordinatorHostObject>(this).persistent_stores;
    let count: u32 = msg![env; stores count];
    msg![env; stores objectAtIndex:(count - 1)]
}

- (bool)removePersistentStore:(id)store  // NSPersistentStore*
                        error:(MutPtr<id>)_error {
    let stores = env.objc.borrow::<NSPersistentStoreCoordinatorHostObject>(this).persistent_stores;
    () = msg![env; stores removeObject:store];
    true
}

- (bool)migratePersistentStore:(id)_store  // NSPersistentStore*
                         toURL:(id)_url    // NSURL*
                       options:(id)_opts  // NSDictionary*
                       withType:(id)_type // NSString*
                          error:(MutPtr<id>)_err {
    clear_error_out_parameter(env, _err); // NSError**
    log!("NSPersistentStoreCoordinator migratePersistentStore: stubbed -> false");
    false
}

- (id)metadataForPersistentStore:(id)_store { // NSPersistentStore*
    nil
}

- (())setMetadata:(id)_metadata                 // NSDictionary*
    forPersistentStore:(id)_store {             // NSPersistentStore*
    log!("NSPersistentStoreCoordinator setMetadata:forPersistentStore: stubbed");
}

- (())lock {
    log_dbg!("NSPersistentStoreCoordinator lock: stubbed");
}

- (())unlock {
    log_dbg!("NSPersistentStoreCoordinator unlock: stubbed");
}

- (bool)tryLock {
    true
}

- (id)executeRequest:(id)_request          // NSPersistentStoreRequest*
         withContext:(id)_context          // NSManagedObjectContext*
               error:(MutPtr<id>)_error {
    clear_error_out_parameter(env, _error);         // NSError**
    log!("NSPersistentStoreCoordinator executeRequest:withContext:error: stubbed -> nil");
    nil
}

@end

// =========================================================================
// NSPersistentStore
// =========================================================================

@implementation NSPersistentStore: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(NSPersistentStoreHostObject {
        type_: nil,
        url: nil,
        options: nil,
        coordinator: nil,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)initWithPersistentStoreCoordinator:(id)coordinator // NSPersistentStoreCoordinator*
                       configurationName:(id)_config     // NSString*
                                     URL:(id)url         // NSURL*
                                 options:(id)options {   // NSDictionary*
    // coordinator is weak — no retain.
    env.objc.borrow_mut::<NSPersistentStoreHostObject>(this).coordinator = coordinator;
    retain(env, url);
    retain(env, options);
    env.objc.borrow_mut::<NSPersistentStoreHostObject>(this).url     = url;
    env.objc.borrow_mut::<NSPersistentStoreHostObject>(this).options = options;
    this
}

- (())dealloc {
    let host = env.objc.borrow::<NSPersistentStoreHostObject>(this);
    let (type_, url, options) = (host.type_, host.url, host.options);
    release(env, type_);
    release(env, url);
    release(env, options);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)URL {
    env.objc.borrow::<NSPersistentStoreHostObject>(this).url
}

- (id)options {
    env.objc.borrow::<NSPersistentStoreHostObject>(this).options
}

- (id)type {
    env.objc.borrow::<NSPersistentStoreHostObject>(this).type_
}

- (id)persistentStoreCoordinator {
    env.objc.borrow::<NSPersistentStoreHostObject>(this).coordinator
}

- (id)identifier {
    nil
}

- (id)configurationName {
    nil
}

- (bool)isReadOnly {
    false
}

- (())setReadOnly:(bool)_value {
    // Stub.
}

- (id)metadata {
    nil
}

- (())setMetadata:(id)_metadata {
    // Stub.
}

- (bool)didAddToPersistentStoreCoordinator:(id)_coordinator {
    true
}

- (())willRemoveFromPersistentStoreCoordinator:(id)_coordinator {
    // Stub.
}

@end

// =========================================================================
// NSManagedObjectModel
// =========================================================================

@implementation NSManagedObjectModel: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let entities = msg_class![env; NSMutableArray new];
    let host_object = Box::new(NSManagedObjectModelHostObject {
        entities,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

+ (id)mergedModelFromBundles:(id)_bundles { // NSArray<NSBundle*>*
    log!("NSManagedObjectModel mergedModelFromBundles: stubbed -> empty model");
    let model: id = msg_class![env; NSManagedObjectModel new];
    autorelease(env, model)
}

+ (id)modelByMergingModels:(id)_models { // NSArray<NSManagedObjectModel*>*
    log!("NSManagedObjectModel modelByMergingModels: stubbed -> empty model");
    let model: id = msg_class![env; NSManagedObjectModel new];
    autorelease(env, model)
}

- (id)init {
    this
}

- (id)initWithContentsOfURL:(id)_url { // NSURL*
    log!("NSManagedObjectModel initWithContentsOfURL: stubbed -> empty model");
    this
}

- (())dealloc {
    let entities = env.objc.borrow::<NSManagedObjectModelHostObject>(this).entities;
    release(env, entities);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)entities {
    env.objc.borrow::<NSManagedObjectModelHostObject>(this).entities
}

- (())setEntities:(id)entities {
    let old = env.objc.borrow::<NSManagedObjectModelHostObject>(this).entities;
    release(env, old);
    retain(env, entities);
    env.objc.borrow_mut::<NSManagedObjectModelHostObject>(this).entities = entities;
}

- (id)entitiesByName {
    let by_name: id = msg_class![env; NSMutableDictionary new];
    let entities: id = env.objc.borrow::<NSManagedObjectModelHostObject>(this).entities;
    let count: NSUInteger = msg![env; entities count];
    for i in 0..count {
        let entity: id = msg![env; entities objectAtIndex:i];
        if entity == nil {
            continue;
        }
        let name: id = msg![env; entity name];
        if name != nil {
            () = msg![env; by_name setObject:entity forKey:name];
        }
    }
    autorelease(env, by_name)
}

- (id)configurations {
    msg_class![env; NSArray new]
}

- (id)entitiesForConfiguration:(id)_configuration {
    msg_class![env; NSArray new]
}

- (bool)isConfiguration:(id)_configuration
    compatibleWithStoreMetadata:(id)_metadata {
    true
}

@end

// =========================================================================
// NSManagedObjectContext
// =========================================================================

@implementation NSManagedObjectContext: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(NSManagedObjectContextHostObject {
        persistent_store_coordinator: nil,
        parent_context: nil,
        undo_manager: nil,
        merge_policy: nil,
        registered_objects: msg_class![env; NSMutableSet new],
        inserted_objects: msg_class![env; NSMutableSet new],
        deleted_objects: msg_class![env; NSMutableSet new],
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)init {
    this
}

- (id)initWithConcurrencyType:(NSInteger)_concurrency_type {
    this
}

- (())dealloc {
    let host = env.objc.borrow::<NSManagedObjectContextHostObject>(this);
    let (psc, undo, registered, inserted, deleted) = (
        host.persistent_store_coordinator,
        host.undo_manager,
        host.registered_objects,
        host.inserted_objects,
        host.deleted_objects,
    );
    release(env, psc);
    release(env, undo);
    // The sets release the objects they retain.
    release(env, registered);
    release(env, inserted);
    release(env, deleted);
    // parent_context and merge_policy are weak / unretained
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)persistentStoreCoordinator {
    env.objc.borrow::<NSManagedObjectContextHostObject>(this).persistent_store_coordinator
}

- (())setPersistentStoreCoordinator:(id)coordinator {
    let old = env.objc.borrow::<NSManagedObjectContextHostObject>(this)
        .persistent_store_coordinator;
    release(env, old);
    retain(env, coordinator);
    env.objc.borrow_mut::<NSManagedObjectContextHostObject>(this)
        .persistent_store_coordinator = coordinator;
}

- (id)parentContext {
    env.objc.borrow::<NSManagedObjectContextHostObject>(this).parent_context
}

- (())setParentContext:(id)ctx {
    // Weak — no retain.
    env.objc.borrow_mut::<NSManagedObjectContextHostObject>(this).parent_context = ctx;
}

- (id)undoManager {
    env.objc.borrow::<NSManagedObjectContextHostObject>(this).undo_manager
}

- (())setUndoManager:(id)undo_manager {
    let old = env.objc.borrow::<NSManagedObjectContextHostObject>(this).undo_manager;
    release(env, old);
    retain(env, undo_manager);
    env.objc.borrow_mut::<NSManagedObjectContextHostObject>(this).undo_manager = undo_manager;
}

- (id)mergePolicy {
    env.objc.borrow::<NSManagedObjectContextHostObject>(this).merge_policy
}

- (())setMergePolicy:(id)policy {
    // Weak — no retain.
    env.objc.borrow_mut::<NSManagedObjectContextHostObject>(this).merge_policy = policy;
}

- (bool)hasChanges {
    let host = env.objc.borrow::<NSManagedObjectContextHostObject>(this);
    let (inserted, deleted) = (host.inserted_objects, host.deleted_objects);
    let inserted_count: NSUInteger = msg![env; inserted count];
    let deleted_count: NSUInteger = msg![env; deleted count];
    inserted_count != 0 || deleted_count != 0
}

- (bool)save:(MutPtr<id>)error {
    // Persistence is not implemented, so "saving" means accepting the changes
    // into the context's snapshot: saved objects stop counting as inserted, so
    // `-hasChanges` answers NO afterwards, as it does on a real context.
    clear_error_out_parameter(env, error);
    let host = env.objc.borrow::<NSManagedObjectContextHostObject>(this);
    let (registered, inserted, deleted) = (
        host.registered_objects,
        host.inserted_objects,
        host.deleted_objects,
    );
    let deleted_enumerator: id = msg![env; deleted objectEnumerator];
    if deleted_enumerator != nil {
        loop {
            let object: id = msg![env; deleted_enumerator nextObject];
            if object == nil {
                break;
            }
            () = msg![env; registered removeObject:object];
        }
    }
    () = msg![env; inserted removeAllObjects];
    () = msg![env; deleted removeAllObjects];
    log_dbg!("NSManagedObjectContext save: -> true (persistence is not implemented)");
    true
}

- (())reset {
    log!("NSManagedObjectContext reset: stubbed");
}

- (())rollback {
    log!("NSManagedObjectContext rollback: stubbed");
}

- (())undo {
    log!("NSManagedObjectContext undo: stubbed");
}

- (())redo {
    log!("NSManagedObjectContext redo: stubbed");
}

- (id)executeFetchRequest:(id)_request // NSFetchRequest*
                    error:(MutPtr<id>)_error {
    log!("NSManagedObjectContext executeFetchRequest:error: stubbed -> empty array");
    let arr: id = msg_class![env; NSArray new];
    autorelease(env, arr)
}

- (NSUInteger)countForFetchRequest:(id)_request // NSFetchRequest*
                              error:(MutPtr<id>)_error {
    0
}

- (id)existingObjectWithID:(id)_object_id // NSManagedObjectID*
                     error:(MutPtr<id>)_error {
    clear_error_out_parameter(env, _error);   // NSError**
    nil
}

- (id)objectWithID:(id)_object_id { // NSManagedObjectID*
    nil
}

- (id)objectRegisteredForID:(id)_object_id { // NSManagedObjectID*
    nil
}

- (())insertObject:(id)object { // NSManagedObject*
    if object == nil {
        return;
    }
    // Apple: an object inserted into a context is registered, retained by the
    // context (the sets do the retaining) and counts as inserted until the
    // next save.
    let host = env.objc.borrow::<NSManagedObjectContextHostObject>(this);
    let (registered, inserted, deleted) = (
        host.registered_objects,
        host.inserted_objects,
        host.deleted_objects,
    );
    () = msg![env; registered addObject:object];
    () = msg![env; inserted addObject:object];
    () = msg![env; deleted removeObject:object];
}

- (())deleteObject:(id)object { // NSManagedObject*
    if object == nil {
        return;
    }
    let host = env.objc.borrow::<NSManagedObjectContextHostObject>(this);
    let (registered, inserted, deleted) = (
        host.registered_objects,
        host.inserted_objects,
        host.deleted_objects,
    );
    // Register the deletion before dropping the context's reference, so the
    // object cannot be deallocated mid-call.
    () = msg![env; deleted addObject:object];
    () = msg![env; inserted removeObject:object];
    () = msg![env; registered removeObject:object];
}

- (())refreshObject:(id)_object mergeChanges:(bool)_merge {
    log!("NSManagedObjectContext refreshObject:mergeChanges: stubbed");
}

- (())processPendingChanges {
    log_dbg!("NSManagedObjectContext processPendingChanges: stubbed");
}

- (())mergeChangesFromContextDidSaveNotification:(id)_notification {
    log!("NSManagedObjectContext mergeChangesFromContextDidSaveNotification: stubbed");
}

- (())setMergeChangesFromContextDidSaveNotificationOnMain:(bool)_value {
    // Stub.
}

- (())performBlock:(id)block { // void (^)(void)
    // Core Data would run the block on the context's private queue. The
    // emulator does not model Core Data queues, so the block runs here,
    // synchronously. Not running it at all (the old behaviour) is a hang:
    // start-up code that waits for the block's side effects (a flag, a
    // semaphore, an object the block inserts) waits forever.
    if block != nil {
        call_void_block(env, block.cast_void());
    }
}

- (())performBlockAndWait:(id)block { // void (^)(void)
    // "…AndWait" is defined as running the block and not returning until it
    // has finished, so running it inline is exactly the documented behaviour.
    if block != nil {
        call_void_block(env, block.cast_void());
    }
}

- (id)registeredObjects {
    env.objc.borrow::<NSManagedObjectContextHostObject>(this).registered_objects
}

- (id)insertedObjects {
    env.objc.borrow::<NSManagedObjectContextHostObject>(this).inserted_objects
}

- (id)updatedObjects {
    // Nothing tracks updates yet, and there is no storage to compare against.
    msg_class![env; NSSet new]
}

- (id)deletedObjects {
    env.objc.borrow::<NSManagedObjectContextHostObject>(this).deleted_objects
}

@end

// =========================================================================
// NSManagedObject
// =========================================================================

@implementation NSManagedObject: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(NSManagedObjectHostObject {
        entity: nil,
        managed_object_context: nil,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)initWithEntity:(id)entity                        // NSEntityDescription*
insertIntoManagedObjectContext:(id)context {           // NSManagedObjectContext*
    retain(env, entity);
    env.objc.borrow_mut::<NSManagedObjectHostObject>(this).entity = entity;
    // context is weak.
    env.objc.borrow_mut::<NSManagedObjectHostObject>(this).managed_object_context = context;
    if context != nil {
        () = msg![env; context insertObject:this];
    }
    this
}

- (())dealloc {
    let entity = env.objc.borrow::<NSManagedObjectHostObject>(this).entity;
    release(env, entity);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)entity {
    env.objc.borrow::<NSManagedObjectHostObject>(this).entity
}

- (id)managedObjectContext {
    env.objc.borrow::<NSManagedObjectHostObject>(this).managed_object_context
}

- (id)objectID {
    nil
}

- (bool)isInserted   { true  }
- (bool)isUpdated    { false }
- (bool)isDeleted    { false }
- (bool)isFault      { false }
- (bool)hasChanges   { false }

- (id)valueForKey:(id)_key {
    log_dbg!("NSManagedObject valueForKey: stubbed -> nil");
    nil
}

- (())setValue:(id)_value forKey:(id)_key {
    log_dbg!("NSManagedObject setValue:forKey: stubbed");
}

- (())willAccessValueForKey:(id)_key  {}
- (())didAccessValueForKey:(id)_key   {}
- (())willChangeValueForKey:(id)_key  {}
- (())didChangeValueForKey:(id)_key   {}

- (id)primitiveValueForKey:(id)_key   { nil }
- (())setPrimitiveValue:(id)_value forKey:(id)_key {}

- (id)changedValues         { msg_class![env; NSDictionary new] }
- (id)changedValuesForCurrentEvent { msg_class![env; NSDictionary new] }
- (id)committedValuesForKeys:(id)_keys { msg_class![env; NSDictionary new] }

- (())validateForInsert:(id)_error {}
- (())validateForUpdate:(id)_error {}
- (())validateForDelete:(id)_error {}
- (())awakeFromInsert {}
- (())awakeFromFetch  {}
- (())prepareForDeletion {}
- (())willSave  {}
- (())didSave   {}
- (())willTurnIntoFault {}
- (())didTurnIntoFault  {}

@end

// =========================================================================
// NSEntityDescription
// =========================================================================

@implementation NSEntityDescription: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let props = msg_class![env; NSMutableDictionary new];
    let host_object = Box::new(NSEntityDescriptionHostObject {
        name: nil,
        managed_object_class_name: nil,
        properties: nil,
        properties_by_name: props,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

// Per Apple, `+entityForName:inManagedObjectContext:` returns the entity
// registered in the context's model, or `nil` if the model has no such entity.
// Returning `nil` unconditionally (the old behaviour) made every Core Data
// client fail at start-up: Ghost Toasters asks for its entity, gets `nil`, and
// never progresses past that point on the boot path.
+ (id)entityForName:(id)name                        // NSString*
    inManagedObjectContext:(id)context {            // NSManagedObjectContext*
    entity_named(env, name, context)
}

+ (id)insertNewObjectForEntityForName:(id)name      // NSString*
             inManagedObjectContext:(id)context {   // NSManagedObjectContext*
    let entity = entity_named(env, name, context);
    if entity == nil {
        return nil;
    }
    // Apple: the new object is an instance of the entity's
    // `managedObjectClassName` (a subclass of `NSManagedObject`, usually), and
    // is inserted into the context as part of creation:
    // `[[cls alloc] initWithEntity:entity insertIntoManagedObjectContext:ctx]`.
    // A guest subclass inherits the host `+allocWithZone:`, so it gets the
    // right host object; only fall back to `NSManagedObject` itself if the
    // name does not resolve to a subclass (which would leave the object
    // without the state these methods need).
    let managed_object_class: Class = env.objc.get_known_class("NSManagedObject", &mut env.mem);
    let mut object_class: Class = managed_object_class;
    let class_name: id = msg![env; entity managedObjectClassName];
    if class_name != nil {
        let class_name = ns_string::to_rust_string(env, class_name).into_owned();
        if !class_name.is_empty() {
            if let Some(candidate) = env.objc.try_get_known_class(&class_name, &mut env.mem) {
                if env.objc.class_is_subclass_of(candidate, managed_object_class) {
                    object_class = candidate;
                }
            }
        }
    }
    let object: id = msg![env; object_class alloc];
    msg![env; object initWithEntity:entity insertIntoManagedObjectContext:context]
}

- (())dealloc {
    let host = env.objc.borrow::<NSEntityDescriptionHostObject>(this);
    let (name, class_name, properties, props) = (
        host.name,
        host.managed_object_class_name,
        host.properties,
        host.properties_by_name,
    );
    release(env, name);
    release(env, class_name);
    release(env, properties);
    release(env, props);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)name {
    env.objc.borrow::<NSEntityDescriptionHostObject>(this).name
}

- (())setName:(id)name {
    let old = env.objc.borrow::<NSEntityDescriptionHostObject>(this).name;
    release(env, old);
    retain(env, name);
    env.objc.borrow_mut::<NSEntityDescriptionHostObject>(this).name = name;
}

- (id)managedObjectClassName {
    env.objc.borrow::<NSEntityDescriptionHostObject>(this).managed_object_class_name
}

- (())setManagedObjectClassName:(id)name {
    let old = env.objc.borrow::<NSEntityDescriptionHostObject>(this).managed_object_class_name;
    release(env, old);
    retain(env, name);
    env.objc.borrow_mut::<NSEntityDescriptionHostObject>(this).managed_object_class_name = name;
}

- (id)propertiesByName {
    env.objc.borrow::<NSEntityDescriptionHostObject>(this).properties_by_name
}

- (id)properties {
    // Apple declares `-properties` as returning an `NSArray` of property
    // descriptions, so it is derived from the by-name index rather than
    // echoing back whatever `-setProperties:` happened to receive. Like Core
    // Data, the order is unspecified.
    let by_name = env.objc.borrow::<NSEntityDescriptionHostObject>(this).properties_by_name;
    msg![env; by_name allValues]
}

- (())setProperties:(id)properties {
    // Apple: "Sets the properties of the receiver." An app that builds its
    // model in code calls this during start-up; dropping the properties (the
    // old behaviour) left the entity empty and every property lookup failing.
    let old = env.objc.borrow::<NSEntityDescriptionHostObject>(this).properties;
    release(env, old);
    retain(env, properties);
    let index: id = msg_class![env; NSMutableDictionary new];
    index_properties_by_name(env, properties, index);
    let old_index = env.objc.borrow_mut::<NSEntityDescriptionHostObject>(this)
        .properties_by_name;
    release(env, old_index);
    env.objc.borrow_mut::<NSEntityDescriptionHostObject>(this).properties = properties;
    env.objc.borrow_mut::<NSEntityDescriptionHostObject>(this).properties_by_name = index;
}

- (id)attributesByName {
    let by_name = env.objc.borrow::<NSEntityDescriptionHostObject>(this).properties_by_name;
    properties_of_class(env, by_name, "NSAttributeDescription")
}

- (id)relationshipsByName {
    let by_name = env.objc.borrow::<NSEntityDescriptionHostObject>(this).properties_by_name;
    properties_of_class(env, by_name, "NSRelationshipDescription")
}

- (id)subentities   { msg_class![env; NSArray new] }
- (id)subentitiesByName { msg_class![env; NSDictionary new] }
- (id)superentity   { nil }
- (bool)isAbstract  { false }
- (id)userInfo      { nil }

@end

// =========================================================================
// NSFetchRequest
// =========================================================================

@implementation NSFetchRequest: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(NSFetchRequestHostObject {
        entity: nil,
        entity_name: nil,
        predicate: nil,
        sort_descriptors: nil,
        fetch_limit: 0,
        fetch_offset: 0,
        fetch_batch_size: 0,
        returns_objects_as_faults: true,
        includes_subentities: true,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

+ (id)fetchRequestWithEntityName:(id)name { // NSString*
    let req: id = msg_class![env; NSFetchRequest alloc];
    let req: id = msg![env; req initWithEntityName:name];
    autorelease(env, req)
}

- (id)initWithEntityName:(id)name { // NSString*
    retain(env, name);
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).entity_name = name;
    this
}

- (())dealloc {
    let host = env.objc.borrow::<NSFetchRequestHostObject>(this);
    let (entity, entity_name, predicate, sort) = (
        host.entity, host.entity_name, host.predicate, host.sort_descriptors
    );
    release(env, entity);
    release(env, entity_name);
    release(env, predicate);
    release(env, sort);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)entity {
    env.objc.borrow::<NSFetchRequestHostObject>(this).entity
}

- (())setEntity:(id)entity {
    let old = env.objc.borrow::<NSFetchRequestHostObject>(this).entity;
    release(env, old);
    retain(env, entity);
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).entity = entity;
}

- (id)entityName {
    env.objc.borrow::<NSFetchRequestHostObject>(this).entity_name
}

- (id)predicate {
    env.objc.borrow::<NSFetchRequestHostObject>(this).predicate
}

- (())setPredicate:(id)predicate {
    let old = env.objc.borrow::<NSFetchRequestHostObject>(this).predicate;
    release(env, old);
    retain(env, predicate);
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).predicate = predicate;
}

- (id)sortDescriptors {
    env.objc.borrow::<NSFetchRequestHostObject>(this).sort_descriptors
}

- (())setSortDescriptors:(id)descs {
    let old = env.objc.borrow::<NSFetchRequestHostObject>(this).sort_descriptors;
    release(env, old);
    retain(env, descs);
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).sort_descriptors = descs;
}

- (NSUInteger)fetchLimit {
    env.objc.borrow::<NSFetchRequestHostObject>(this).fetch_limit as NSUInteger
}

- (())setFetchLimit:(NSUInteger)limit {
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).fetch_limit = limit as u64;
}

- (NSUInteger)fetchOffset {
    env.objc.borrow::<NSFetchRequestHostObject>(this).fetch_offset as NSUInteger
}

- (())setFetchOffset:(NSUInteger)offset {
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).fetch_offset = offset as u64;
}

- (NSUInteger)fetchBatchSize {
    env.objc.borrow::<NSFetchRequestHostObject>(this).fetch_batch_size as NSUInteger
}

- (())setFetchBatchSize:(NSUInteger)size {
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).fetch_batch_size = size as u64;
}

- (bool)returnsObjectsAsFaults {
    env.objc.borrow::<NSFetchRequestHostObject>(this).returns_objects_as_faults
}

- (())setReturnsObjectsAsFaults:(bool)value {
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).returns_objects_as_faults = value;
}

- (bool)includesSubentities {
    env.objc.borrow::<NSFetchRequestHostObject>(this).includes_subentities
}

- (())setIncludesSubentities:(bool)value {
    env.objc.borrow_mut::<NSFetchRequestHostObject>(this).includes_subentities = value;
}

- (bool)includesPendingChanges    { true  }
- (())setIncludesPendingChanges:(bool)_v {}

- (bool)returnsDistinctResults    { false }
- (())setReturnsDistinctResults:(bool)_v {}

- (id)propertiesToFetch           { nil }
- (())setPropertiesToFetch:(id)_v {}

- (id)propertiesToGroupBy         { nil }
- (())setPropertiesToGroupBy:(id)_v {}

- (id)havingPredicate             { nil }
- (())setHavingPredicate:(id)_v   {}

- (NSInteger)resultType           { 0 } // NSManagedObjectResultType
- (())setResultType:(NSInteger)_v {}

@end

// =========================================================================
// NSPropertyDescription (abstract base for NSAttributeDescription,
// NSRelationshipDescription, NSFetchedPropertyDescription)
// =========================================================================

@implementation NSPropertyDescription: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(NSPropertyDescriptionHostObject {
        name: nil,
        optional: true,
        transient: false,
        user_info: nil,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (())dealloc {
    let host = env.objc.borrow::<NSPropertyDescriptionHostObject>(this);
    let (name, user_info) = (host.name, host.user_info);
    release(env, name);
    release(env, user_info);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (id)name {
    env.objc.borrow::<NSPropertyDescriptionHostObject>(this).name
}

- (())setName:(id)name {
    let old = env.objc.borrow::<NSPropertyDescriptionHostObject>(this).name;
    release(env, old);
    retain(env, name);
    env.objc.borrow_mut::<NSPropertyDescriptionHostObject>(this).name = name;
}

- (bool)isOptional {
    env.objc.borrow::<NSPropertyDescriptionHostObject>(this).optional
}

- (())setOptional:(bool)optional {
    env.objc.borrow_mut::<NSPropertyDescriptionHostObject>(this).optional = optional;
}

- (bool)isTransient {
    env.objc.borrow::<NSPropertyDescriptionHostObject>(this).transient
}

- (())setTransient:(bool)transient {
    env.objc.borrow_mut::<NSPropertyDescriptionHostObject>(this).transient = transient;
}

- (id)userInfo {
    env.objc.borrow::<NSPropertyDescriptionHostObject>(this).user_info
}

- (())setUserInfo:(id)info {
    let old = env.objc.borrow::<NSPropertyDescriptionHostObject>(this).user_info;
    release(env, old);
    retain(env, info);
    env.objc.borrow_mut::<NSPropertyDescriptionHostObject>(this).user_info = info;
}

- (id)entity { nil }
- (id)validationPredicates { msg_class![env; NSArray new] }
- (id)validationWarnings { msg_class![env; NSArray new] }
- (bool)isIndexed { false }
- (())setIndexed:(bool)_v {}
- (bool)isIndexedBySpotlight { false }
- (())setIndexedBySpotlight:(bool)_v {}
- (bool)isStoredInExternalRecord { false }
- (())setStoredInExternalRecord:(bool)_v {}

@end

// =========================================================================
// NSAttributeDescription
// Per Apple: <https://developer.apple.com/documentation/coredata/nsattributedescription>
// Describes a Core Data entity attribute (its type, default value, etc.).
// =========================================================================

@implementation NSAttributeDescription: NSPropertyDescription

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(NSAttributeDescriptionHostObject {
        superclass: NSPropertyDescriptionHostObject::default(),
        attribute_type: 0, // NSUndefinedAttributeType
        default_value: nil,
        allows_external_binary_data_storage: false,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (())dealloc {
    let host = env.objc.borrow::<NSAttributeDescriptionHostObject>(this);
    let (default_value, name, user_info) = (
        host.default_value,
        host.superclass.name,
        host.superclass.user_info,
    );
    release(env, default_value);
    // `-dealloc` of the superclass does not run by itself for host objects, so
    // the embedded `NSPropertyDescription` state is released here.
    release(env, name);
    release(env, user_info);
    env.objc.dealloc_object(this, &mut env.mem)
}

- (NSInteger)attributeType {
    env.objc.borrow::<NSAttributeDescriptionHostObject>(this).attribute_type
}

- (())setAttributeType:(NSInteger)attr_type {
    env.objc.borrow_mut::<NSAttributeDescriptionHostObject>(this).attribute_type = attr_type;
}

- (id)defaultValue {
    env.objc.borrow::<NSAttributeDescriptionHostObject>(this).default_value
}

- (())setDefaultValue:(id)value {
    let old = env.objc.borrow::<NSAttributeDescriptionHostObject>(this).default_value;
    release(env, old);
    retain(env, value);
    env.objc.borrow_mut::<NSAttributeDescriptionHostObject>(this).default_value = value;
}

- (id)attributeValueClassName { nil }
- (())setAttributeValueClassName:(id)_name {}

- (id)valueTransformerName { nil }
- (())setValueTransformerName:(id)_name {}

- (bool)allowsExternalBinaryDataStorage {
    env.objc.borrow::<NSAttributeDescriptionHostObject>(this).allows_external_binary_data_storage
}

- (())setAllowsExternalBinaryDataStorage:(bool)value {
    env.objc.borrow_mut::<NSAttributeDescriptionHostObject>(this).allows_external_binary_data_storage = value;
}

@end

// =========================================================================
// NSRelationshipDescription
// =========================================================================

@implementation NSRelationshipDescription: NSPropertyDescription

- (id)destinationEntity { nil }
- (())setDestinationEntity:(id)_entity {}
- (id)inverseRelationship { nil }
- (())setInverseRelationship:(id)_rel {}
- (NSInteger)deleteRule { 0 } // NSNoActionDeleteRule
- (())setDeleteRule:(NSInteger)_rule {}
- (bool)isToMany { false }
- (NSUInteger)maxCount { 0 }
- (())setMaxCount:(NSUInteger)_count {}
- (NSUInteger)minCount { 0 }
- (())setMinCount:(NSUInteger)_count {}
- (bool)isOrdered { false }
- (())setOrdered:(bool)_v {}

@end

};

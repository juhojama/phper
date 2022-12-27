// Copyright (c) 2022 PHPER Framework Team
// PHPER is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2. You may obtain a copy of Mulan PSL v2 at:
//          http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

//! Apis relate to [crate::sys::zend_module_entry].

use crate::{
    c_str_ptr,
    classes::ClassEntity,
    constants::Constant,
    errors::Throwable,
    functions::{Function, FunctionEntity, FunctionEntry},
    ini,
    sys::*,
    types::Scalar,
    utils::ensure_end_with_zero,
    values::ZVal,
};
use std::{
    collections::HashMap,
    ffi::CString,
    mem::{replace, size_of, take, transmute, zeroed},
    os::raw::{c_int, c_uchar, c_uint, c_ushort},
    ptr::{null, null_mut},
    rc::Rc,
    sync::atomic::{AtomicPtr, Ordering},
};

// TODO Should not hold global prt here.
static GLOBAL_MODULE: AtomicPtr<Module> = AtomicPtr::new(null_mut());

pub(crate) fn read_global_module<R>(f: impl FnOnce(&Module) -> R) -> R {
    let module = GLOBAL_MODULE.load(Ordering::SeqCst);
    f(unsafe { module.as_ref() }.expect("GLOBAL_MODULE is null"))
}

pub(crate) fn write_global_module<R>(f: impl FnOnce(&mut Module) -> R) -> R {
    let module = GLOBAL_MODULE.load(Ordering::SeqCst);
    f(unsafe { module.as_mut() }.expect("GLOBAL_MODULE is null"))
}

unsafe extern "C" fn module_startup(r#type: c_int, module_number: c_int) -> c_int {
    let args = ModuleContext::new(r#type, module_number);
    write_global_module(|module| {
        args.register_ini_entries(ini::entries(take(&mut module.ini_entities)));
        for constant in &module.constants {
            constant.register(&args);
        }
        for class_entity in &mut module.class_entities {
            let ce = class_entity.init();
            class_entity.declare_properties(ce);
        }
        let module_init = replace(&mut module.module_init, None);
        match module_init {
            Some(f) => f(args) as c_int,
            None => 1,
        }
    })
}

unsafe extern "C" fn module_shutdown(r#type: c_int, module_number: c_int) -> c_int {
    let args = ModuleContext::new(r#type, module_number);
    args.unregister_ini_entries();
    write_global_module(|module| {
        let module_shutdown = replace(&mut module.module_shutdown, None);
        match module_shutdown {
            Some(f) => f(args) as c_int,
            None => 1,
        }
    })
}

unsafe extern "C" fn request_startup(r#type: c_int, request_number: c_int) -> c_int {
    read_global_module(|module| match &module.request_init {
        Some(f) => f(ModuleContext::new(r#type, request_number)) as c_int,
        None => 1,
    })
}

unsafe extern "C" fn request_shutdown(r#type: c_int, request_number: c_int) -> c_int {
    read_global_module(|module| match &module.request_shutdown {
        Some(f) => f(ModuleContext::new(r#type, request_number)) as c_int,
        None => 1,
    })
}

unsafe extern "C" fn module_info(zend_module: *mut zend_module_entry) {
    read_global_module(|module| {
        php_info_print_table_start();
        if !module.version.as_bytes().is_empty() {
            php_info_print_table_row(2, c_str_ptr!("version"), module.version.as_ptr());
        }
        if !module.author.as_bytes().is_empty() {
            php_info_print_table_row(2, c_str_ptr!("authors"), module.author.as_ptr());
        }
        for (key, value) in &module.infos {
            php_info_print_table_row(2, key.as_ptr(), value.as_ptr());
        }
        php_info_print_table_end();
    });
    display_ini_entries(zend_module);
}

/// Builder for registering PHP Module.
pub struct Module {
    name: CString,
    version: CString,
    author: CString,
    module_init: Option<Box<dyn FnOnce(ModuleContext) -> bool + Send + Sync>>,
    module_shutdown: Option<Box<dyn FnOnce(ModuleContext) -> bool + Send + Sync>>,
    request_init: Option<Box<dyn Fn(ModuleContext) -> bool + Send + Sync>>,
    request_shutdown: Option<Box<dyn Fn(ModuleContext) -> bool + Send + Sync>>,
    function_entities: Vec<FunctionEntity>,
    class_entities: Vec<ClassEntity<()>>,
    constants: Vec<Constant>,
    ini_entities: Vec<ini::IniEntity>,
    infos: HashMap<CString, CString>,
}

impl Module {
    /// Construct the `Module` with base metadata.
    pub fn new(
        name: impl Into<String>, version: impl Into<String>, author: impl Into<String>,
    ) -> Self {
        Self {
            name: ensure_end_with_zero(name),
            version: ensure_end_with_zero(version),
            author: ensure_end_with_zero(author),
            module_init: None,
            module_shutdown: None,
            request_init: None,
            request_shutdown: None,
            function_entities: vec![],
            class_entities: Default::default(),
            constants: Default::default(),
            ini_entities: Default::default(),
            infos: Default::default(),
        }
    }

    /// Register `MINIT` hook.
    pub fn on_module_init(
        &mut self, func: impl FnOnce(ModuleContext) -> bool + Send + Sync + 'static,
    ) {
        self.module_init = Some(Box::new(func));
    }

    /// Register `MSHUTDOWN` hook.
    pub fn on_module_shutdown(
        &mut self, func: impl FnOnce(ModuleContext) -> bool + Send + Sync + 'static,
    ) {
        self.module_shutdown = Some(Box::new(func));
    }

    /// Register `RINIT` hook.
    pub fn on_request_init(
        &mut self, func: impl Fn(ModuleContext) -> bool + Send + Sync + 'static,
    ) {
        self.request_init = Some(Box::new(func));
    }

    /// Register `RSHUTDOWN` hook.
    pub fn on_request_shutdown(
        &mut self, func: impl Fn(ModuleContext) -> bool + Send + Sync + 'static,
    ) {
        self.request_shutdown = Some(Box::new(func));
    }

    /// Register function to module.
    pub fn add_function<F, Z, E>(
        &mut self, name: impl Into<String>, handler: F,
    ) -> &mut FunctionEntity
    where
        F: Fn(&mut [ZVal]) -> Result<Z, E> + 'static,
        Z: Into<ZVal> + 'static,
        E: Throwable + 'static,
    {
        self.function_entities
            .push(FunctionEntity::new(name, Rc::new(Function::new(handler))));
        self.function_entities.last_mut().unwrap()
    }

    /// Register class to module.
    pub fn add_class<T>(&mut self, class: ClassEntity<T>) {
        self.class_entities.push(unsafe { transmute(class) });
    }

    /// Register constant to module.
    pub fn add_constant(&mut self, name: impl Into<String>, value: impl Into<Scalar>) {
        self.constants.push(Constant::new(name, value));
    }

    /// Register ini configuration to module.
    pub fn add_ini(
        &mut self, name: impl Into<String>, default_value: impl ini::IntoIniValue,
        policy: ini::Policy,
    ) {
        self.ini_entities
            .push(ini::IniEntity::new(name, default_value, policy));
    }

    /// Register info item.
    ///
    /// # Panics
    ///
    /// Panic if key or value contains '\0'.
    pub fn add_info(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = CString::new(key.into()).expect("key contains '\0'");
        let value = CString::new(value.into()).expect("value contains '\0'");
        self.infos.insert(key, value);
    }

    /// Leak memory to generate `zend_module_entry` pointer.
    #[doc(hidden)]
    pub unsafe fn module_entry(self) -> *const zend_module_entry {
        assert!(!self.name.as_bytes().is_empty(), "module name must be set");
        assert!(
            !self.version.as_bytes().is_empty(),
            "module version must be set"
        );

        let entry: Box<zend_module_entry> = Box::new(zend_module_entry {
            size: size_of::<zend_module_entry>() as c_ushort,
            zend_api: ZEND_MODULE_API_NO as c_uint,
            zend_debug: ZEND_DEBUG as c_uchar,
            zts: USING_ZTS as c_uchar,
            ini_entry: null(),
            deps: null(),
            name: self.name.as_ptr().cast(),
            functions: self.function_entries(),
            module_startup_func: Some(module_startup),
            module_shutdown_func: Some(module_shutdown),
            request_startup_func: Some(request_startup),
            request_shutdown_func: Some(request_shutdown),
            info_func: Some(module_info),
            version: null(),
            globals_size: 0,
            #[cfg(phper_zts)]
            globals_id_ptr: std::ptr::null_mut(),
            #[cfg(not(phper_zts))]
            globals_ptr: std::ptr::null_mut(),
            globals_ctor: None,
            globals_dtor: None,
            post_deactivate_func: None,
            module_started: 0,
            type_: 0,
            handle: null_mut(),
            module_number: 0,
            build_id: phper_get_zend_module_build_id(),
        });

        let entry = Box::into_raw(entry);
        GLOBAL_MODULE.store(Box::into_raw(Box::new(self)), Ordering::SeqCst);
        entry
    }

    fn function_entries(&self) -> *const zend_function_entry {
        if self.function_entities.is_empty() {
            return null();
        }

        let mut entries = Vec::new();
        for f in &self.function_entities {
            entries.push(unsafe { FunctionEntry::from_function_entity(f) });
        }
        entries.push(unsafe { zeroed::<zend_function_entry>() });

        Box::into_raw(entries.into_boxed_slice()).cast()
    }
}

/// Module context owned module type and number.
pub struct ModuleContext {
    #[allow(dead_code)]
    pub(crate) r#type: c_int,
    pub(crate) module_number: c_int,
}

impl ModuleContext {
    const fn new(r#type: c_int, module_number: c_int) -> Self {
        Self {
            r#type,
            module_number,
        }
    }

    pub(crate) fn register_ini_entries(&self, ini_entries: *const zend_ini_entry_def) {
        unsafe {
            zend_register_ini_entries(ini_entries, self.module_number);
        }
    }

    pub(crate) fn unregister_ini_entries(&self) {
        unsafe {
            zend_unregister_ini_entries(self.module_number);
        }
    }
}

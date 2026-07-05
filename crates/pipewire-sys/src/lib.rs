// Copyright The pipewire-rs Contributors.
// SPDX-License-Identifier: MIT

//! FFI bindings for PipeWire, resolved at runtime.
//!
//! Types and constants come from bindgen as usual, but no link against
//! `libpipewire-0.3` is emitted. Every function chatt's PipeWire stack uses
//! is declared in [`pw_dlopen_api!`] below, which expands to a table of
//! function pointers resolved from `libpipewire-0.3.so.0` via `dlopen` on
//! first use, plus a free function per symbol delegating through the table.
//! The free functions shadow the unused bindgen externs, so dependent crates
//! compile unchanged while the binary runs on systems without PipeWire
//! installed. Call [`library_available`] before any other function; calling
//! into PipeWire when the library is absent aborts the process.

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(unpredictable_function_pointer_comparisons)]
#[allow(unnecessary_transmutes)]
// The extern declarations are intentionally unused: the dlopen wrappers below
// shadow every function the dependent crates call, and an unreferenced extern
// creates no link requirement.
#[allow(dead_code)]
#[allow(clippy::all)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
pub use bindings::*;

use std::sync::OnceLock;

use spa_sys::{spa_dict, spa_direction, spa_hook, spa_pod};

/// Declares the runtime-loaded PipeWire API surface.
///
/// Entries after `@variadic` are C-variadic symbols. Their table entry is a
/// variadic function pointer so calls use the correct ABI, while the public
/// wrapper exposes only the fixed arity the `pipewire` crate actually calls.
macro_rules! pw_dlopen_api {
    (
        $(fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;)*
        @variadic
        $(fn $vname:ident($($varg:ident: $vty:ty),* $(,)?) $(-> $vret:ty)?;)*
    ) => {
        struct PwApi {
            $($name: unsafe extern "C" fn($($ty),*) $(-> $ret)?,)*
            $($vname: unsafe extern "C" fn($($vty),*, ...) $(-> $vret)?,)*
        }

        impl PwApi {
            unsafe fn load(lib: &libloading::Library) -> Result<Self, libloading::Error> {
                unsafe {
                    Ok(Self {
                        $($name: *lib.get(concat!(stringify!($name), "\0").as_bytes())?,)*
                        $($vname: *lib.get(concat!(stringify!($vname), "\0").as_bytes())?,)*
                    })
                }
            }
        }

        $(pub unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
            unsafe { (api().$name)($($arg),*) }
        })*
        $(pub unsafe fn $vname($($varg: $vty),*) $(-> $vret)? {
            unsafe { (api().$vname)($($varg),*) }
        })*
    };
}

fn load() -> Option<PwApi> {
    for name in ["libpipewire-0.3.so.0", "libpipewire-0.3.so"] {
        let Ok(lib) = (unsafe { libloading::Library::new(name) }) else {
            continue;
        };
        let Ok(api) = (unsafe { PwApi::load(&lib) }) else {
            continue;
        };
        // The mapping must outlive every resolved function pointer, and the
        // process never unloads PipeWire, so the handle is deliberately leaked.
        std::mem::forget(lib);
        return Some(api);
    }
    None
}

fn loaded() -> Option<&'static PwApi> {
    static API: OnceLock<Option<PwApi>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

/// Attempts to load `libpipewire-0.3` and resolve the full API table,
/// returning whether PipeWire is usable in this process. The result is
/// computed once and cached.
pub fn library_available() -> bool {
    loaded().is_some()
}

fn api() -> &'static PwApi {
    let Some(api) = loaded() else {
        eprintln!("pipewire-sys: called into PipeWire but libpipewire-0.3 is not loadable");
        std::process::abort();
    };
    api
}

pw_dlopen_api! {
    fn pw_client_info_free(info: *mut pw_client_info);
    fn pw_context_connect(
        context: *mut pw_context,
        properties: *mut pw_properties,
        user_data_size: usize,
    ) -> *mut pw_core;
    fn pw_context_connect_fd(
        context: *mut pw_context,
        fd: ::std::os::raw::c_int,
        properties: *mut pw_properties,
        user_data_size: usize,
    ) -> *mut pw_core;
    fn pw_context_destroy(context: *mut pw_context);
    fn pw_context_get_properties(context: *mut pw_context) -> *const pw_properties;
    fn pw_context_new(
        main_loop: *mut pw_loop,
        props: *mut pw_properties,
        user_data_size: usize,
    ) -> *mut pw_context;
    fn pw_context_update_properties(
        context: *mut pw_context,
        dict: *const spa_dict,
    ) -> ::std::os::raw::c_int;
    fn pw_core_disconnect(core: *mut pw_core) -> ::std::os::raw::c_int;
    fn pw_deinit();
    fn pw_device_info_free(info: *mut pw_device_info);
    fn pw_factory_info_free(info: *mut pw_factory_info);
    fn pw_init(argc: *mut ::std::os::raw::c_int, argv: *mut *mut *mut ::std::os::raw::c_char);
    fn pw_link_info_free(info: *mut pw_link_info);
    fn pw_loop_destroy(loop_: *mut pw_loop);
    fn pw_loop_new(props: *const spa_dict) -> *mut pw_loop;
    fn pw_main_loop_destroy(loop_: *mut pw_main_loop);
    fn pw_main_loop_get_loop(loop_: *mut pw_main_loop) -> *mut pw_loop;
    fn pw_main_loop_new(props: *const spa_dict) -> *mut pw_main_loop;
    fn pw_main_loop_quit(loop_: *mut pw_main_loop) -> ::std::os::raw::c_int;
    fn pw_main_loop_run(loop_: *mut pw_main_loop) -> ::std::os::raw::c_int;
    fn pw_module_info_free(info: *mut pw_module_info);
    fn pw_node_info_free(info: *mut pw_node_info);
    fn pw_port_info_free(info: *mut pw_port_info);
    fn pw_properties_clear(properties: *mut pw_properties);
    fn pw_properties_copy(properties: *const pw_properties) -> *mut pw_properties;
    fn pw_properties_free(properties: *mut pw_properties);
    fn pw_properties_get(
        properties: *const pw_properties,
        key: *const ::std::os::raw::c_char,
    ) -> *const ::std::os::raw::c_char;
    fn pw_properties_new_dict(dict: *const spa_dict) -> *mut pw_properties;
    fn pw_properties_set(
        properties: *mut pw_properties,
        key: *const ::std::os::raw::c_char,
        value: *const ::std::os::raw::c_char,
    ) -> ::std::os::raw::c_int;
    fn pw_proxy_add_listener(
        proxy: *mut pw_proxy,
        listener: *mut spa_hook,
        events: *const pw_proxy_events,
        data: *mut ::std::os::raw::c_void,
    );
    fn pw_proxy_destroy(proxy: *mut pw_proxy);
    fn pw_proxy_get_id(proxy: *mut pw_proxy) -> u32;
    fn pw_proxy_get_type(
        proxy: *mut pw_proxy,
        version: *mut u32,
    ) -> *const ::std::os::raw::c_char;
    fn pw_stream_add_listener(
        stream: *mut pw_stream,
        listener: *mut spa_hook,
        events: *const pw_stream_events,
        data: *mut ::std::os::raw::c_void,
    );
    fn pw_stream_connect(
        stream: *mut pw_stream,
        direction: spa_direction,
        target_id: u32,
        flags: pw_stream_flags,
        params: *mut *const spa_pod,
        n_params: u32,
    ) -> ::std::os::raw::c_int;
    fn pw_stream_dequeue_buffer(stream: *mut pw_stream) -> *mut pw_buffer;
    fn pw_stream_destroy(stream: *mut pw_stream);
    fn pw_stream_disconnect(stream: *mut pw_stream) -> ::std::os::raw::c_int;
    fn pw_stream_flush(stream: *mut pw_stream, drain: bool) -> ::std::os::raw::c_int;
    fn pw_stream_get_name(stream: *mut pw_stream) -> *const ::std::os::raw::c_char;
    fn pw_stream_get_node_id(stream: *mut pw_stream) -> u32;
    fn pw_stream_get_properties(stream: *mut pw_stream) -> *const pw_properties;
    fn pw_stream_get_state(
        stream: *mut pw_stream,
        error: *mut *const ::std::os::raw::c_char,
    ) -> pw_stream_state;
    fn pw_stream_get_time(stream: *mut pw_stream, time: *mut pw_time) -> ::std::os::raw::c_int;
    fn pw_stream_get_time_n(
        stream: *mut pw_stream,
        time: *mut pw_time,
        size: usize,
    ) -> ::std::os::raw::c_int;
    fn pw_stream_is_driving(stream: *mut pw_stream) -> bool;
    fn pw_stream_new(
        core: *mut pw_core,
        name: *const ::std::os::raw::c_char,
        props: *mut pw_properties,
    ) -> *mut pw_stream;
    fn pw_stream_queue_buffer(
        stream: *mut pw_stream,
        buffer: *mut pw_buffer,
    ) -> ::std::os::raw::c_int;
    fn pw_stream_set_active(stream: *mut pw_stream, active: bool) -> ::std::os::raw::c_int;
    fn pw_stream_trigger_process(stream: *mut pw_stream) -> ::std::os::raw::c_int;
    fn pw_stream_update_params(
        stream: *mut pw_stream,
        params: *mut *const spa_pod,
        n_params: u32,
    ) -> ::std::os::raw::c_int;
    fn pw_thread_loop_accept(loop_: *mut pw_thread_loop);
    fn pw_thread_loop_destroy(loop_: *mut pw_thread_loop);
    fn pw_thread_loop_get_loop(loop_: *mut pw_thread_loop) -> *mut pw_loop;
    fn pw_thread_loop_get_time(
        loop_: *mut pw_thread_loop,
        abstime: *mut timespec,
        timeout: i64,
    ) -> ::std::os::raw::c_int;
    fn pw_thread_loop_in_thread(loop_: *mut pw_thread_loop) -> bool;
    fn pw_thread_loop_lock(loop_: *mut pw_thread_loop);
    fn pw_thread_loop_new(
        name: *const ::std::os::raw::c_char,
        props: *const spa_dict,
    ) -> *mut pw_thread_loop;
    fn pw_thread_loop_signal(loop_: *mut pw_thread_loop, wait_for_accept: bool);
    fn pw_thread_loop_start(loop_: *mut pw_thread_loop) -> ::std::os::raw::c_int;
    fn pw_thread_loop_stop(loop_: *mut pw_thread_loop);
    fn pw_thread_loop_timed_wait(
        loop_: *mut pw_thread_loop,
        wait_max_sec: ::std::os::raw::c_int,
    ) -> ::std::os::raw::c_int;
    fn pw_thread_loop_timed_wait_full(
        loop_: *mut pw_thread_loop,
        abstime: *const timespec,
    ) -> ::std::os::raw::c_int;
    fn pw_thread_loop_unlock(loop_: *mut pw_thread_loop);
    fn pw_thread_loop_wait(loop_: *mut pw_thread_loop);
    @variadic
    fn pw_properties_new(key: *const ::std::os::raw::c_char) -> *mut pw_properties;
    fn pw_stream_set_control(
        stream: *mut pw_stream,
        id: u32,
        n_values: u32,
        values: *mut f32
    ) -> ::std::os::raw::c_int;
    fn pw_stream_set_error(
        stream: *mut pw_stream,
        res: ::std::os::raw::c_int,
        error: *const ::std::os::raw::c_char
    ) -> ::std::os::raw::c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_loads_and_resolves_all_symbols() {
        if !library_available() {
            return;
        }
        unsafe {
            pw_init(std::ptr::null_mut(), std::ptr::null_mut());
            pw_deinit();
        }
    }
}

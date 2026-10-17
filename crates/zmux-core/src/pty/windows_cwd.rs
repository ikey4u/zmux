use std::os::windows::ffi::OsStringExt;

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        Diagnostics::{
            Debug::ReadProcessMemory,
            ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
                PROCESSENTRY32W, TH32CS_SNAPPROCESS,
            },
        },
        Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
        },
    },
};

const PROCESS_BASIC_INFORMATION: u32 = 0;

#[repr(C)]
struct ProcessBasicInformation {
    exit_status: isize,
    peb_base_address: *mut core::ffi::c_void,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

#[repr(C)]
struct Peb {
    inherited_address_space: u8,
    read_image_file_exec_options: u8,
    being_debugged: u8,
    bit_field: u8,
    mutant: isize,
    image_base_address: *mut core::ffi::c_void,
    ldr: *mut core::ffi::c_void,
    process_parameters: *mut core::ffi::c_void,
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct CurDir {
    dos_path: UnicodeString,
    handle: isize,
}

#[repr(C)]
struct RtlUserProcessParameters {
    maximum_length: u32,
    length: u32,
    flags: u32,
    debug_flags: u32,
    console_handle: *mut core::ffi::c_void,
    console_flags: u32,
    _console_padding: u32,
    standard_input: isize,
    standard_output: isize,
    standard_error: isize,
    current_directory: CurDir,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(
        process_handle: HANDLE,
        process_information_class: u32,
        process_information: *mut core::ffi::c_void,
        process_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

pub fn process_current_dir(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    read_process_cwd_for_pid(pid).or_else(|| {
        descendant_pids(pid)
            .into_iter()
            .find_map(read_process_cwd_for_pid)
    })
}

fn read_process_cwd_for_pid(pid: u32) -> Option<String> {
    let handle = open_process(pid)?;
    let cwd = read_process_cwd(handle);
    unsafe {
        CloseHandle(handle);
    }
    cwd
}

fn open_process(pid: u32) -> Option<HANDLE> {
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ;
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        None
    } else {
        Some(handle)
    }
}

fn read_process_cwd(handle: HANDLE) -> Option<String> {
    let mut info = ProcessBasicInformation {
        exit_status: 0,
        peb_base_address: core::ptr::null_mut(),
        affinity_mask: 0,
        base_priority: 0,
        unique_process_id: 0,
        inherited_from_unique_process_id: 0,
    };
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            PROCESS_BASIC_INFORMATION,
            (&mut info as *mut ProcessBasicInformation).cast(),
            core::mem::size_of::<ProcessBasicInformation>() as u32,
            core::ptr::null_mut(),
        )
    };
    if status != 0 || info.peb_base_address.is_null() {
        return None;
    }

    let mut peb = Peb {
        inherited_address_space: 0,
        read_image_file_exec_options: 0,
        being_debugged: 0,
        bit_field: 0,
        mutant: 0,
        image_base_address: core::ptr::null_mut(),
        ldr: core::ptr::null_mut(),
        process_parameters: core::ptr::null_mut(),
    };
    if !read_process_memory(
        handle,
        info.peb_base_address,
        (&mut peb as *mut Peb).cast(),
        core::mem::size_of::<Peb>(),
    ) {
        return None;
    }
    if peb.process_parameters.is_null() {
        return None;
    }

    let mut params = RtlUserProcessParameters {
        maximum_length: 0,
        length: 0,
        flags: 0,
        debug_flags: 0,
        console_handle: core::ptr::null_mut(),
        console_flags: 0,
        _console_padding: 0,
        standard_input: 0,
        standard_output: 0,
        standard_error: 0,
        current_directory: CurDir {
            dos_path: UnicodeString {
                length: 0,
                maximum_length: 0,
                buffer: core::ptr::null_mut(),
            },
            handle: 0,
        },
    };
    if !read_process_memory(
        handle,
        peb.process_parameters,
        (&mut params as *mut RtlUserProcessParameters).cast(),
        core::mem::size_of::<RtlUserProcessParameters>(),
    ) {
        return None;
    }

    read_unicode_string(handle, &params.current_directory.dos_path)
        .map(normalize_windows_path)
}

fn read_unicode_string(
    handle: HANDLE,
    value: &UnicodeString,
) -> Option<String> {
    if value.buffer.is_null() || value.length == 0 {
        return None;
    }
    let byte_len = value.length as usize;
    let wchar_len = byte_len / 2;
    let mut buf = vec![0u16; wchar_len + 1];
    if !read_process_memory(
        handle,
        value.buffer.cast(),
        buf.as_mut_ptr().cast(),
        byte_len,
    ) {
        return None;
    }
    let path = std::ffi::OsString::from_wide(&buf[..wchar_len]);
    let path = path.to_string_lossy().into_owned();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

fn normalize_windows_path(path: String) -> String {
    let path = path.trim_end_matches('\0');
    let path = path
        .strip_prefix(r"\??\")
        .or_else(|| path.strip_prefix(r"\\?\"))
        .unwrap_or(path);
    path.replace('/', "\\")
}

fn descendant_pids(root_pid: u32) -> Vec<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() {
        return Vec::new();
    }

    let mut entries = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { core::mem::zeroed() }
    };
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) };
    while ok != 0 {
        entries.push((entry.th32ProcessID, entry.th32ParentProcessID));
        ok = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    unsafe {
        CloseHandle(snapshot);
    }

    let mut descendants = Vec::new();
    let mut frontier = vec![root_pid];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in &entries {
            if *ppid == parent && *pid != parent {
                descendants.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    descendants
}

fn read_process_memory(
    handle: HANDLE,
    address: *const core::ffi::c_void,
    buffer: *mut core::ffi::c_void,
    size: usize,
) -> bool {
    let mut read = 0;
    unsafe {
        ReadProcessMemory(handle, address, buffer, size, &mut read) != 0
            && read == size
    }
}

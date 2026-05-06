use std::ffi::OsStr;

pub fn set_env_var<K: AsRef<OsStr>, V: AsRef<OsStr>>(key: K, value: V) {
    // SAFETY: callers must only use this from serialized tests that mutate process environment.
    unsafe { std::env::set_var(key, value) };
}

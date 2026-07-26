use crate::error::Result;
use crate::scope::{AsyncScope, LocalAsyncScope};
use std::ops::AsyncFnOnce;

/// 把 `async |s| ..` 字面量的参数类型钉成一个作用域引用。
///
/// 闭包在宏体内被立刻调用，所以除 `'r` 之外的生命周期可以自由推导 —— 这也正是宏无法直接
/// 转发给 [`RuntimeCtx::scope`](crate::runtime::RuntimeCtx::scope) 的原因，详见 `scope!`。
#[doc(hidden)]
pub fn _constrain<'g, 'env, R, F, TExtra>(f: F) -> F
where
    F: for<'r> AsyncFnOnce(&'r AsyncScope<'r, 'g, 'env, TExtra>) -> R,
{
    f
}

#[doc(hidden)]
pub fn _constrain_local<'g, 'env, R, F, TExtra>(f: F) -> F
where
    F: for<'r> AsyncFnOnce(&'r LocalAsyncScope<'r, 'g, 'env, TExtra>) -> R,
{
    f
}

/// 钉住宏展开结果的错误类型（否则 `Ok(res)` 里的 `E` 无从推导）。
#[doc(hidden)]
pub fn _constrain_result<T>(r: Result<T>) -> Result<T> {
    r
}

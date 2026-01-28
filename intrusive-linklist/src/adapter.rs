use crate::link::Link;
use core::ptr::NonNull;

/// 适配器 Trait，用于定义对象与 Link 之间的映射关系
///
/// # Safety
/// 实现者必须保证 get_link 和 get_value 的转换是正确且内存安全的。
pub unsafe trait Adapter {
    /// 链表存储的数据类型 (例如 WaiterNode)
    type Value: ?Sized;

    /// 给定数据指针，返回该数据中 Link 字段的指针
    unsafe fn get_link(&self, value: NonNull<Self::Value>) -> NonNull<Link>;

    /// 给定 Link 指针，返回包含该 Link 的数据指针
    unsafe fn get_value(&self, link: NonNull<Link>) -> NonNull<Self::Value>;
}

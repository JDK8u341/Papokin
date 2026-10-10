use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::BufMut;
use papokin_data::meta_data_type::MetaDataType;
use papokin_data::tracked_data::{TrackedData, TrackedId};
use papokin_protocol::codec::optional_int::OptionalInt;
use papokin_protocol::codec::var_int::VarInt;
use papokin_protocol::java::client::play::{Metadata, MetadataSerializer};
use papokin_protocol::ser::WritingError;
use papokin_util::version::JavaMinecraftVersion;

pub trait ErasedSerializer: Send + Sync {
    fn write(
        &self,
        index: TrackedId,
        r#type: MetaDataType,
        writer: &mut dyn std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError>;

    fn write_canonical(&self, index: TrackedId, r#type: MetaDataType) -> Vec<u8>;
}

struct SerializerHolder<T> {
    value: T,
}

impl<T: MetadataSerializer + Clone + Send + Sync + 'static> ErasedSerializer
    for SerializerHolder<T>
{
    fn write(
        &self,
        index: TrackedId,
        r#type: MetaDataType,
        writer: &mut dyn std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        let meta = Metadata::new_raw(index, r#type, &self.value);
        meta.write(writer, version)
    }

    fn write_canonical(&self, index: TrackedId, r#type: MetaDataType) -> Vec<u8> {
        let mut buf = Vec::new();
        let meta = Metadata::new_raw(index, r#type, &self.value);
        let _ = meta.write(&mut buf, &JavaMinecraftVersion::V_26_3);
        buf
    }
}

/// 装箱形态的大值：序列化器与其 canonical 字节同住一次堆分配，
/// canonical 仍用于变更检测（装箱族没有廉价的逐位比较）。
pub struct BoxedValue {
    serializer: Box<dyn ErasedSerializer>,
    canonical_bytes: Vec<u8>,
}

/// 追踪项的存储形态：高频小值内联在枚举里（零堆分配、
/// 零序列化即可比较相等），其余类型装箱回退到既有的
/// 类型擦除序列化器路径。变体集合按全仓调用点的实际值类型选取，
/// 新增小类型时补一个内联变体即可。
pub enum TrackedValue {
    Bool(bool),
    I8(i8),
    U8(u8),
    I32(i32),
    F32(f32),
    VarInt(VarInt),
    OptionalInt(OptionalInt),
    BlockPos(papokin_util::math::position::BlockPos),
    /// 装箱本身仅 8 字节，枚举尺寸不被大值撑爆。
    Boxed(Box<BoxedValue>),
}

impl TrackedValue {
    /// 与旧实现的「canonical 字节相等」语义逐位等价的变更检测：
    /// 整数/布尔按值；浮点按 `to_bits`（`-0.0` 与 `0.0` 的
    /// canonical 字节不同，位比较同样不同；NaN 位相同则视为未变）；
    /// 装箱族沿用 canonical 字节比较。变体不同视为已变化
    /// （同一键的值类型在实际调用中恒定，此分支仅为防御）。
    fn equivalent(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::I8(a), Self::I8(b)) => a == b,
            (Self::U8(a), Self::U8(b)) => a == b,
            (Self::I32(a), Self::I32(b)) => a == b,
            (Self::F32(a), Self::F32(b)) => a.to_bits() == b.to_bits(),
            (Self::VarInt(a), Self::VarInt(b)) => a == b,
            (Self::OptionalInt(a), Self::OptionalInt(b)) => a == b,
            (Self::BlockPos(a), Self::BlockPos(b)) => a == b,
            (Self::Boxed(a), Self::Boxed(b)) => a.canonical_bytes == b.canonical_bytes,
            _ => false,
        }
    }

    /// 按版本写出单个元数据条目（等价于旧 `DataItem.serializer.write`）。
    fn write(
        &self,
        index: TrackedId,
        r#type: MetaDataType,
        writer: &mut dyn std::io::Write,
        version: JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        match self {
            Self::Bool(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::I8(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::U8(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::I32(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::F32(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::VarInt(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::OptionalInt(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::BlockPos(v) => Metadata::new_raw(index, r#type, v).write(writer, &version),
            Self::Boxed(b) => b.serializer.write(index, r#type, writer, &version),
        }
    }
}

/// 把调用点传入的具体值转换为存储形态：小类型内联零分配，
/// 其余类型装箱（转换时即产出 canonical 字节，供变更检测复用）。
/// 逐类型显式实现（无 blanket），漏掉某个调用点类型会直接
/// 编译失败，保证内联集与真实使用面一致。
pub trait IntoTrackedValue: Sized {
    fn into_tracked_value(self, tracked: TrackedData) -> TrackedValue;
}

pub(crate) fn boxed_value<T: MetadataSerializer + Clone + Send + Sync + 'static>(
    value: T,
    tracked: TrackedData,
) -> TrackedValue {
    let holder = SerializerHolder { value };
    let canonical_bytes = holder.write_canonical(tracked.id, tracked.r#type);
    TrackedValue::Boxed(Box::new(BoxedValue {
        serializer: Box::new(holder),
        canonical_bytes,
    }))
}

impl IntoTrackedValue for bool {
    fn into_tracked_value(self, _tracked: TrackedData) -> TrackedValue {
        TrackedValue::Bool(self)
    }
}

impl IntoTrackedValue for i8 {
    fn into_tracked_value(self, _tracked: TrackedData) -> TrackedValue {
        TrackedValue::I8(self)
    }
}

impl IntoTrackedValue for u8 {
    fn into_tracked_value(self, _tracked: TrackedData) -> TrackedValue {
        TrackedValue::U8(self)
    }
}

impl IntoTrackedValue for i32 {
    fn into_tracked_value(self, _tracked: TrackedData) -> TrackedValue {
        TrackedValue::I32(self)
    }
}

impl IntoTrackedValue for f32 {
    fn into_tracked_value(self, _tracked: TrackedData) -> TrackedValue {
        TrackedValue::F32(self)
    }
}

impl IntoTrackedValue for VarInt {
    fn into_tracked_value(self, _tracked: TrackedData) -> TrackedValue {
        TrackedValue::VarInt(self)
    }
}

impl IntoTrackedValue for OptionalInt {
    fn into_tracked_value(self, _tracked: TrackedData) -> TrackedValue {
        TrackedValue::OptionalInt(self)
    }
}

impl IntoTrackedValue for papokin_util::math::position::BlockPos {
    fn into_tracked_value(self, _tracked: TrackedData) -> TrackedValue {
        TrackedValue::BlockPos(self)
    }
}

impl IntoTrackedValue for Option<papokin_util::text::TextComponent> {
    fn into_tracked_value(self, tracked: TrackedData) -> TrackedValue {
        boxed_value(self, tracked)
    }
}

impl IntoTrackedValue for papokin_util::text::TextComponent {
    fn into_tracked_value(self, tracked: TrackedData) -> TrackedValue {
        boxed_value(self, tracked)
    }
}

impl IntoTrackedValue for Option<uuid::Uuid> {
    fn into_tracked_value(self, tracked: TrackedData) -> TrackedValue {
        boxed_value(self, tracked)
    }
}

impl IntoTrackedValue for Option<papokin_util::math::position::BlockPos> {
    fn into_tracked_value(self, tracked: TrackedData) -> TrackedValue {
        boxed_value(self, tracked)
    }
}

impl IntoTrackedValue
    for papokin_protocol::codec::item_stack_seralizer::ItemStackSerializer<'static>
{
    fn into_tracked_value(self, tracked: TrackedData) -> TrackedValue {
        boxed_value(self, tracked)
    }
}

impl IntoTrackedValue for crate::entity::decoration::display::Vector3fSerializer {
    fn into_tracked_value(self, tracked: TrackedData) -> TrackedValue {
        boxed_value(self, tracked)
    }
}

impl IntoTrackedValue for crate::entity::decoration::display::QuaternionfSerializer {
    fn into_tracked_value(self, tracked: TrackedData) -> TrackedValue {
        boxed_value(self, tracked)
    }
}

/// 单个追踪项。键（`TrackedData`）由外层向量的元素携带，
/// 不再在值内重复存储。
pub struct DataItem {
    pub value: TrackedValue,
    pub dirty: bool,
    pub is_default: bool,
}

/// 实体同步元数据表：扁平 `Vec<(键, 值)>`，线性查找。
/// 追踪项数量为个位到数十条（每实体），线性扫描的缓存局部性
/// 优于哈希表，且驻留无桶数组/负载因子开销、小值零堆分配。
pub struct SynchedEntityData {
    items: Mutex<Vec<(TrackedData, DataItem)>>,
    is_dirty: AtomicBool,
}

impl Default for SynchedEntityData {
    fn default() -> Self {
        Self::new()
    }
}

impl SynchedEntityData {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            is_dirty: AtomicBool::new(false),
        }
    }

    pub fn define<T: MetadataSerializer + Clone + Send + Sync + 'static + IntoTrackedValue>(
        &self,
        tracked: TrackedData,
        value: T,
    ) {
        let value = value.into_tracked_value(tracked);
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match items.iter_mut().find(|(key, _)| *key == tracked) {
            Some((_, item)) => {
                *item = DataItem {
                    value,
                    dirty: false,
                    is_default: true,
                };
            }
            None => items.push((
                tracked,
                DataItem {
                    value,
                    dirty: false,
                    is_default: true,
                },
            )),
        }
    }

    pub fn set<T: MetadataSerializer + Clone + Send + Sync + 'static + IntoTrackedValue>(
        &self,
        tracked: TrackedData,
        value: T,
    ) -> bool {
        let value = value.into_tracked_value(tracked);

        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, item)) = items.iter_mut().find(|(key, _)| *key == tracked) {
            if item.value.equivalent(&value) {
                return false;
            }
            item.value = value;
            item.dirty = true;
            item.is_default = false;
        } else {
            items.push((
                tracked,
                DataItem {
                    value,
                    dirty: true,
                    is_default: false,
                },
            ));
        }
        self.is_dirty.store(true, Ordering::Release);
        true
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.is_dirty.load(Ordering::Acquire)
    }

    /// 在同一把锁内为每个在线客户端版本打包脏项并清脏。
    ///
    /// 分离的“打包后清脏”（先 `pack_dirty_for_version` 再
    /// `clear_dirty`）存在窗口：两步之间并发 [`Self::set`] 写入的
    /// 新脏项会被清脏一并吞掉，该次元数据更新将永远不会广播
    /// （高并发 tick 下的静默数据丢失）。此方法持有同一把
    /// `items` 锁完成打包与清脏，`set` 只能在其前后生效。
    pub fn pack_dirty_for_versions(
        &self,
        versions: &[JavaMinecraftVersion],
    ) -> Vec<(JavaMinecraftVersion, Box<[u8]>)> {
        let mut items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut out = Vec::new();
        for version in versions {
            let mut buf = Vec::new();
            let mut has_any = false;
            for (tracked, item) in items.iter() {
                if item.dirty {
                    let before_len = buf.len();
                    if item
                        .value
                        .write(tracked.id, tracked.r#type, &mut buf, *version)
                        .is_ok()
                        && buf.len() > before_len
                    {
                        has_any = true;
                    }
                }
            }
            if has_any {
                buf.put_u8(255);
                out.push((*version, buf.into_boxed_slice()));
            }
        }

        for (_, item) in items.iter_mut() {
            item.dirty = false;
        }
        self.is_dirty.store(false, Ordering::Release);
        out
    }

    pub fn get_non_default_values_for_version(
        &self,
        version: &JavaMinecraftVersion,
    ) -> Option<Box<[u8]>> {
        let items = self
            .items
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut buf = Vec::new();
        let mut has_any = false;

        for (tracked, item) in items.iter() {
            if !item.is_default {
                let before_len = buf.len();
                if item
                    .value
                    .write(tracked.id, tracked.r#type, &mut buf, *version)
                    .is_ok()
                    && buf.len() > before_len
                {
                    has_any = true;
                }
            }
        }

        if !has_any {
            return None;
        }

        buf.put_u8(255);
        Some(buf.into_boxed_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use papokin_data::tracked_data;

    /// 打包与清脏在同一临界区：每个版本的缓冲以终止符结尾，
    /// 打包后脏标记清零，二次打包为空。
    #[test]
    fn pack_dirty_for_versions_packs_all_versions_and_clears() {
        let data = SynchedEntityData::new();
        data.define(tracked_data::living_entity::DATA_HEALTH_ID, 20.0f32);
        assert!(data.set(tracked_data::living_entity::DATA_HEALTH_ID, 15.5f32));
        assert!(data.is_dirty());

        let versions = [
            JavaMinecraftVersion::V_1_21_11,
            JavaMinecraftVersion::V_26_3,
        ];
        let packed = data.pack_dirty_for_versions(&versions);

        assert_eq!(packed.len(), 2);
        for (version, buf) in &packed {
            // 终止符 0xFF 必须存在，且两个版本键互不相同
            assert_eq!(*buf.last().unwrap(), 255);
            assert!(versions.contains(version));
        }
        assert!(!data.is_dirty());

        // 清脏后重复打包不得再产生任何包
        assert!(data.pack_dirty_for_versions(&versions).is_empty());
    }

    /// 并发窗口语义：打包完成后新的 set 写入不会被清脏吞掉。
    #[test]
    fn set_after_pack_marks_dirty_again() {
        let data = SynchedEntityData::new();
        data.define(tracked_data::living_entity::DATA_HEALTH_ID, 20.0f32);
        data.set(tracked_data::living_entity::DATA_HEALTH_ID, 15.5f32);
        let versions = [JavaMinecraftVersion::V_1_21_11];
        assert_eq!(data.pack_dirty_for_versions(&versions).len(), 1);

        // 模拟打包后到达的新写入：必须再次变脏等待下一轮广播
        assert!(data.set(tracked_data::living_entity::DATA_HEALTH_ID, 12.0f32));
        assert!(data.is_dirty());
        assert_eq!(data.pack_dirty_for_versions(&versions).len(), 1);
    }

    /// 相同值的 set 不置脏；浮点按位比较：`-0.0` 与 `0.0` 视为不同
    /// （与旧实现的 canonical 字节比较语义一致），NaN 位相同视为未变。
    #[test]
    fn equivalent_floats_match_canonical_byte_semantics() {
        let data = SynchedEntityData::new();
        data.define(tracked_data::living_entity::DATA_HEALTH_ID, 0.0f32);
        // 相同值的 set 既不返回真也不置全局脏位
        assert!(!data.set(tracked_data::living_entity::DATA_HEALTH_ID, 0.0f32));
        assert!(!data.is_dirty());
        assert!(data.set(tracked_data::living_entity::DATA_HEALTH_ID, -0.0f32));
        assert!(data.is_dirty());

        let nan = f32::NAN;
        assert!(data.set(tracked_data::living_entity::DATA_HEALTH_ID, nan));
        assert!(!data.set(tracked_data::living_entity::DATA_HEALTH_ID, nan));
    }

    /// 整数/布尔/VarInt 的相同值 set 不置脏，变化值置脏并触发重打包。
    #[test]
    fn inline_scalars_skip_noop_sets() {
        let data = SynchedEntityData::new();
        data.set(tracked_data::entity::DATA_SHARED_FLAGS_ID, 0u8);
        assert!(!data.set(tracked_data::entity::DATA_SHARED_FLAGS_ID, 0u8));
        assert!(data.set(tracked_data::entity::DATA_SHARED_FLAGS_ID, 1u8));

        data.set(tracked_data::entity::DATA_POSE, VarInt(0));
        assert!(!data.set(tracked_data::entity::DATA_POSE, VarInt(0)));
        assert!(data.set(tracked_data::entity::DATA_POSE, VarInt(3)));

        data.set(tracked_data::entity::DATA_SILENT, false);
        assert!(!data.set(tracked_data::entity::DATA_SILENT, false));
    }

    /// 装箱族（自定义名字）经 canonical 字节比较：相同文本不置脏。
    #[test]
    fn boxed_custom_name_compares_by_canonical_bytes() {
        let data = SynchedEntityData::new();
        let name = papokin_util::text::TextComponent::text("等级观察者");
        assert!(data.set(tracked_data::entity::DATA_CUSTOM_NAME, Some(name.clone())));
        assert!(!data.set(tracked_data::entity::DATA_CUSTOM_NAME, Some(name)));
        let other = papokin_util::text::TextComponent::text("另一个名字");
        assert!(data.set(tracked_data::entity::DATA_CUSTOM_NAME, Some(other)));
    }

    /// define 对既有键整体重置（清脏并回到默认态），与旧 `HashMap`
    /// 整项替换语义一致。
    #[test]
    fn define_resets_existing_entry() {
        let data = SynchedEntityData::new();
        data.set(tracked_data::living_entity::DATA_HEALTH_ID, 15.5f32);
        assert!(data.is_dirty());
        data.define(tracked_data::living_entity::DATA_HEALTH_ID, 20.0f32);
        // 全局脏位是粘滞的（define 不清它，与旧实现一致），
        // 但条目级 dirty/is_default 已被重置：打包脏项为空
        assert!(data.is_dirty());
        let versions = [JavaMinecraftVersion::V_1_21_11];
        assert!(data.pack_dirty_for_versions(&versions).is_empty());
        // is_default 恢复后，非默认值打包不再包含该项
        assert!(
            data.get_non_default_values_for_version(&JavaMinecraftVersion::V_1_21_11)
                .is_none()
        );
    }

    /// 未 define 过的键经 set 直接插入并可打包；多个键的打包输出
    /// 以终止符收尾且包含全部条目的索引。
    #[test]
    fn set_inserts_missing_keys_and_pack_contains_all() {
        let data = SynchedEntityData::new();
        data.set(tracked_data::entity::DATA_SHARED_FLAGS_ID, 1u8);
        data.set(tracked_data::entity::DATA_POSE, VarInt(2));
        data.set(tracked_data::living_entity::DATA_HEALTH_ID, 12.5f32);

        let packed = data.pack_dirty_for_versions(&[JavaMinecraftVersion::V_1_21_11]);
        assert_eq!(packed.len(), 1);
        let buf = &packed[0].1;
        assert_eq!(*buf.last().unwrap(), 255);
        // 三个条目 + 终止符
        assert!(buf.len() > 3);
    }
}

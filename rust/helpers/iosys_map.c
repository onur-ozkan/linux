// SPDX-License-Identifier: GPL-2.0

#include <linux/iosys-map.h>
#include <linux/types.h>

#define rust_iosys_map_rd(type__)                                                       \
	__rust_helper type__                                                            \
	rust_helper_iosys_map_rd_ ## type__(const struct iosys_map *map, size_t offset) \
	{                                                                               \
		return iosys_map_rd(map, offset, type__);                               \
	}
#define rust_iosys_map_wr(type__)                                                       \
	__rust_helper void                                                              \
	rust_helper_iosys_map_wr_ ## type__(const struct iosys_map *map, size_t offset, \
					    type__ value)                               \
	{                                                                               \
		iosys_map_wr(map, offset, type__, value);                               \
	}

rust_iosys_map_rd(u8);
rust_iosys_map_rd(u16);
rust_iosys_map_rd(u32);

rust_iosys_map_wr(u8);
rust_iosys_map_wr(u16);
rust_iosys_map_wr(u32);

#ifdef CONFIG_64BIT
rust_iosys_map_rd(u64);
rust_iosys_map_wr(u64);
#endif

#undef rust_iosys_map_rd
#undef rust_iosys_map_wr

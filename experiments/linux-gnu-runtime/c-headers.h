/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#ifndef PUFFINBOX_EXPERIMENT_ENDIAN_H
#define PUFFINBOX_EXPERIMENT_ENDIAN_H

#ifndef __ASSEMBLER__
#include <stdint.h>
#include <arpa/inet.h>
#include <endian.h>
#include <byteswap.h>

#if !defined(__GNUC__) || !defined(__BYTE_ORDER__)
#error "This experiment requires GNU-compatible byte-swap builtins"
#endif

#undef bswap_16
#undef bswap_32
#undef bswap_64
#define bswap_16(value) __builtin_bswap16((uint16_t)(value))
#define bswap_32(value) __builtin_bswap32((uint32_t)(value))
#define bswap_64(value) __builtin_bswap64((uint64_t)(value))

#undef htobe16
#undef htobe32
#undef htobe64
#undef htole16
#undef htole32
#undef htole64
#undef be16toh
#undef be32toh
#undef be64toh
#undef le16toh
#undef le32toh
#undef le64toh
#undef htons
#undef htonl
#undef ntohs
#undef ntohl

#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
#define htobe16(value) bswap_16(value)
#define htobe32(value) bswap_32(value)
#define htobe64(value) bswap_64(value)
#define htole16(value) ((uint16_t)(value))
#define htole32(value) ((uint32_t)(value))
#define htole64(value) ((uint64_t)(value))
#elif __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__
#define htobe16(value) ((uint16_t)(value))
#define htobe32(value) ((uint32_t)(value))
#define htobe64(value) ((uint64_t)(value))
#define htole16(value) bswap_16(value)
#define htole32(value) bswap_32(value)
#define htole64(value) bswap_64(value)
#else
#error "Unsupported byte order"
#endif
#define be16toh(value) htobe16(value)
#define be32toh(value) htobe32(value)
#define be64toh(value) htobe64(value)
#define le16toh(value) htole16(value)
#define le32toh(value) htole32(value)
#define le64toh(value) htole64(value)
#define htons(value) htobe16(value)
#define htonl(value) htobe32(value)
#define ntohs(value) be16toh(value)
#define ntohl(value) be32toh(value)
#endif
#endif

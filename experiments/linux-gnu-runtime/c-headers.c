/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#include <arpa/inet.h>
#include <byteswap.h>
#include <ctype.h>
#include <endian.h>
#include <locale.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static uint64_t reverse_bytes(uint64_t value, unsigned width) {
    uint64_t result = 0;
    for (unsigned i = 0; i < width; ++i) {
        result = (result << 8) | (value & 255);
        value >>= 8;
    }
    return result;
}

static unsigned character_calls;

static int next_character(int value) {
    ++character_calls;
    return value;
}

int main(int argc, char **argv) {
    if (argc != 2 || atoi(argv[1]) != -12345) return 1;
#ifdef _GNU_SOURCE
    if (sizeof(struct mmsghdr) <= sizeof(struct msghdr) ||
        sizeof(struct in6_pktinfo) < 20) return 4;
#endif
    uint64_t state = UINT64_C(0x94733fac38b1d275);
    const uint16_t endian = 1;
    const int little = *(const unsigned char *)&endian == 1;
    for (unsigned i = 0; i < 4096; ++i) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        uint64_t big = little ? reverse_bytes(state, 8) : state;
        uint64_t small = little ? state : reverse_bytes(state, 8);
        uint32_t value32 = (uint32_t)state;
        uint16_t value16 = (uint16_t)state;
        uint32_t big32 = little ? (uint32_t)reverse_bytes(value32, 4) : value32;
        uint16_t big16 = little ? (uint16_t)reverse_bytes(value16, 2) : value16;
        uint32_t small32 = little ? value32 : (uint32_t)reverse_bytes(value32, 4);
        uint16_t small16 = little ? value16 : (uint16_t)reverse_bytes(value16, 2);
        if (bswap_64(state) != reverse_bytes(state, 8) ||
            bswap_32(value32) != reverse_bytes(value32, 4) ||
            bswap_16(value16) != reverse_bytes(value16, 2) ||
            htobe64(state) != big || be64toh(big) != state ||
            htole64(state) != small || le64toh(small) != state ||
            htobe32(value32) != big32 || be32toh(big32) != value32 ||
            htobe16(value16) != big16 || be16toh(big16) != value16 ||
            htole32(value32) != small32 || le32toh(small32) != value32 ||
            htole16(value16) != small16 || le16toh(small16) != value16 ||
            htonl(value32) != big32 || ntohl(big32) != value32 ||
            htons(value16) != big16 || ntohs(big16) != value16)
            return 2;
        uint64_t sequence = state;
        if (htobe64(sequence++) != big || sequence != state + 1) return 3;
        printf("%016llx %016llx %08x %04x\n", (unsigned long long)big,
               (unsigned long long)small, (unsigned)big32, (unsigned)big16);
    }
    if (setlocale(LC_ALL, "C") == NULL) return 5;
    for (int value = 0; value <= 255; ++value) {
        int lower = value >= 'A' && value <= 'Z' ? value + ('a' - 'A') : value;
        int upper = value >= 'a' && value <= 'z' ? value - ('a' - 'A') : value;
        if (tolower(value) != lower || toupper(value) != upper) return 6;
        printf("ctype %d %d %d\n", value, tolower(value), toupper(value));
    }
    if (tolower(EOF) != EOF || toupper(EOF) != EOF) return 7;
    printf("ctype %d %d %d\n", EOF, tolower(EOF), toupper(EOF));
    if (tolower(next_character('A')) != 'a' || character_calls != 1 ||
        toupper(next_character('z')) != 'Z' || character_calls != 2) return 8;
    printf("ctype-calls %u\n", character_calls);
    return 0;
}

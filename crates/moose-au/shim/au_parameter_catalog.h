#ifndef MOOSE_AU_PARAMETER_CATALOG_H
#define MOOSE_AU_PARAMETER_CATALOG_H
/* Immutable, registration-time indexes. No AudioToolbox dependency, so the
 * exact shared implementation is exercisable on non-Apple builders too. */
#include "au_shim_types.h"
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    const AuParamDescriptor *params;
    uint32_t count, clump_count;
    size_t mask;
    uint32_t *ids, *clumps;
    const char **names;
} MooseAuParameterCatalog;

static void moose_au_catalog_destroy(MooseAuParameterCatalog *catalog) {
    free(catalog->ids); free(catalog->clumps); free(catalog->names);
    memset(catalog, 0, sizeof(*catalog));
}
static size_t moose_au_catalog_group_hash(const char *name) {
    size_t hash = 2166136261u;
    for (const unsigned char *p = (const unsigned char *)name; *p; ++p)
        hash = (hash ^ *p) * (size_t)16777619u;
    return hash;
}
static size_t moose_au_catalog_id_hash(uint32_t id) {
    /* Avalanche high bits too: typed IDs often share a large power-of-two
     * stride, for which low-bit multiplicative hashing forms long chains. */
    id ^= id >> 16; id *= UINT32_C(0x7feb352d);
    id ^= id >> 15; id *= UINT32_C(0x846ca68b);
    id ^= id >> 16;
    return (size_t)id;
}
static int moose_au_catalog_build(MooseAuParameterCatalog *out,
                                const AuParamDescriptor *params, uint32_t count) {
    MooseAuParameterCatalog next = {0};
    if (!count) { *out = next; return 1; }
    if (!params || count == UINT32_MAX || (size_t)count > SIZE_MAX / 2) return 0;
    size_t capacity = 1;
    while (capacity < (size_t)count * 2) {
        if (capacity > SIZE_MAX / 2) return 0;
        capacity *= 2;
    }
    next.params = params; next.count = count; next.mask = capacity - 1;
    next.ids = (uint32_t *)calloc(capacity, sizeof(uint32_t));
    next.clumps = (uint32_t *)calloc(count, sizeof(uint32_t));
    next.names = (const char **)calloc(count, sizeof(const char *));
    uint32_t *groups = (uint32_t *)calloc(capacity, sizeof(uint32_t));
    if (!next.ids || !next.clumps || !next.names || !groups) {
        free(groups); moose_au_catalog_destroy(&next); return 0;
    }
    for (uint32_t i = 0; i < count; ++i) {
        size_t cell = moose_au_catalog_id_hash(params[i].id) & next.mask;
        while (next.ids[cell]) {
            if (params[next.ids[cell] - 1].id == params[i].id) {
                free(groups); moose_au_catalog_destroy(&next); return 0;
            }
            cell = (cell + 1) & next.mask;
        }
        next.ids[cell] = i + 1;
        const char *group = params[i].group;
        if (!group || !*group) continue;
        cell = moose_au_catalog_group_hash(group) & next.mask;
        while (groups[cell] && strcmp(params[groups[cell] - 1].group, group))
            cell = (cell + 1) & next.mask;
        if (!groups[cell]) {
            groups[cell] = i + 1;
            next.names[next.clump_count++] = group;
            next.clumps[i] = next.clump_count;
        } else {
            next.clumps[i] = next.clumps[groups[cell] - 1];
        }
    }
    free(groups);
    *out = next;
    return 1;
}
static uint32_t moose_au_catalog_index(const MooseAuParameterCatalog *catalog, uint32_t id) {
    if (!catalog->count) return UINT32_MAX;
    size_t cell = moose_au_catalog_id_hash(id) & catalog->mask;
    while (catalog->ids[cell]) {
        uint32_t index = catalog->ids[cell] - 1;
        if (catalog->params[index].id == id) return index;
        cell = (cell + 1) & catalog->mask;
    }
    return UINT32_MAX;
}
static uint32_t moose_au_catalog_clump(const MooseAuParameterCatalog *catalog, uint32_t index) {
    return index < catalog->count ? catalog->clumps[index] : 0;
}
static const char *moose_au_catalog_clump_name(const MooseAuParameterCatalog *catalog, uint32_t id) {
    return id && id <= catalog->clump_count ? catalog->names[id - 1] : NULL;
}
#endif

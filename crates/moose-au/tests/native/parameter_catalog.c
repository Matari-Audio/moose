#include "../../shim/au_parameter_catalog.h"
#include <assert.h>
#include <stdio.h>

int main(void) {
    enum { COUNT = 5004, GROUPS = 100 };
    AuParamDescriptor *params = calloc(COUNT, sizeof(*params));
    assert(params);
    char names[GROUPS][32];
    for (uint32_t i = 0; i < GROUPS; ++i)
        snprintf(names[i], sizeof(names[i]), "Modulator %u/Envelope", i + 1);
    for (uint32_t i = 0; i < COUNT; ++i) {
        params[i].id = 0x01000000u + i * 8192u;
        params[i].group = i == COUNT - 1 ? "" : names[i % GROUPS];
    }
    MooseAuParameterCatalog catalog = {0};
    assert(moose_au_catalog_build(&catalog, params, COUNT));
    assert(catalog.clump_count == GROUPS);
    for (uint32_t i = 0; i < COUNT; ++i) {
        assert(moose_au_catalog_index(&catalog, params[i].id) == i);
        uint32_t clump = moose_au_catalog_clump(&catalog, i);
        assert(clump == (i == COUNT - 1 ? 0 : i % GROUPS + 1));
        if (clump) assert(!strcmp(moose_au_catalog_clump_name(&catalog, clump), params[i].group));
    }
    assert(moose_au_catalog_index(&catalog, UINT32_MAX) == UINT32_MAX);
    assert(!moose_au_catalog_clump_name(&catalog, 0));
    assert(!moose_au_catalog_clump_name(&catalog, GROUPS + 1));
    assert(!moose_au_catalog_clump(&catalog, COUNT));
    moose_au_catalog_destroy(&catalog);
    params[COUNT - 1].id = params[0].id;
    assert(!moose_au_catalog_build(&catalog, params, COUNT));
    assert(!catalog.ids && !catalog.clumps && !catalog.names);
    assert(moose_au_catalog_build(&catalog, NULL, 0));
    assert(moose_au_catalog_index(&catalog, 0) == UINT32_MAX);
    free(params);
    puts("AU 5004 descriptors / 100 stable clumps: indexed lookup passed");
    return 0;
}

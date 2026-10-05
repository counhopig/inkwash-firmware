#include "core/refresh_policy.h"
namespace refresh_policy {
Plan Choose(bool base, bool changed, bool full, bool scene) {
    if (full || !base) return Plan::Full;
    if (!changed) return Plan::None;
    return scene ? Plan::Full : Plan::Partial;
}
}  // namespace refresh_policy

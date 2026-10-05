#pragma once
namespace refresh_policy {
enum class Plan { None, Partial, Full };
Plan Choose(bool base_valid, bool changed, bool full_requested, bool scene_changed);
}  // namespace refresh_policy

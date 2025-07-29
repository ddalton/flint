#pragma once

#include <string>

namespace spdk_flint {

// Forward declarations - actual definitions are in app.hpp
enum class AppMode;
struct AppConfig;

// Configuration parsing functions
AppMode parseAppMode(const std::string& mode_str);
AppConfig loadConfigFromEnvironment();

} // namespace spdk_flint 
#pragma once

#include <spdlog/spdlog.h>
#include <spdlog/sinks/stdout_color_sinks.h>
#include <spdlog/sinks/rotating_file_sink.h>
#include <string>
#include <memory>

namespace spdk_flint {

class Logger {
public:
    static void initialize(const std::string& level = "info", 
                          const std::string& log_file = "",
                          size_t max_file_size = 10 * 1024 * 1024,
                          size_t max_files = 3);
    
    static std::shared_ptr<spdlog::logger> get(const std::string& name = "spdk_flint");
    
    static void setLevel(const std::string& level);
    
    static void shutdown();

private:
    static std::shared_ptr<spdlog::logger> default_logger_;
    static bool initialized_;
};

} // namespace spdk_flint

// Convenience macros to replace println! and add structured logging
#define LOG_TRACE(...) spdk_flint::Logger::get()->trace(__VA_ARGS__)
#define LOG_DEBUG(...) spdk_flint::Logger::get()->debug(__VA_ARGS__)
#define LOG_INFO(...) spdk_flint::Logger::get()->info(__VA_ARGS__)
#define LOG_WARN(...) spdk_flint::Logger::get()->warn(__VA_ARGS__)
#define LOG_ERROR(...) spdk_flint::Logger::get()->error(__VA_ARGS__)
#define LOG_CRITICAL(...) spdk_flint::Logger::get()->critical(__VA_ARGS__)

// Context-aware logging macros
#define LOG_CSI_INFO(operation, ...) \
    spdk_flint::Logger::get()->info("[CSI][{}] " __VA_ARGS__, operation)

#define LOG_CSI_ERROR(operation, ...) \
    spdk_flint::Logger::get()->error("[CSI][{}] " __VA_ARGS__, operation)

#define LOG_SPDK_INFO(...) \
    spdk_flint::Logger::get()->info("[SPDK] " __VA_ARGS__)

#define LOG_SPDK_ERROR(...) \
    spdk_flint::Logger::get()->error("[SPDK] " __VA_ARGS__)

#define LOG_DASHBOARD_INFO(...) \
    spdk_flint::Logger::get()->info("[DASHBOARD] " __VA_ARGS__)

#define LOG_DASHBOARD_ERROR(...) \
    spdk_flint::Logger::get()->error("[DASHBOARD] " __VA_ARGS__)

#define LOG_NODE_AGENT_INFO(...) \
    spdk_flint::Logger::get()->info("[NODE_AGENT] " __VA_ARGS__)

#define LOG_NODE_AGENT_ERROR(...) \
    spdk_flint::Logger::get()->error("[NODE_AGENT] " __VA_ARGS__)

#define LOG_CONTROLLER_INFO(...) \
    spdk_flint::Logger::get()->info("[CONTROLLER] " __VA_ARGS__)

#define LOG_CONTROLLER_ERROR(...) \
    spdk_flint::Logger::get()->error("[CONTROLLER] " __VA_ARGS__)

// RPC operation logging (to replace the Rust RPC logging)
#define LOG_RPC_CALL(method, ...) \
    spdk_flint::Logger::get()->debug("[RPC] Calling SPDK method: {} " __VA_ARGS__, method)

#define LOG_RPC_SUCCESS(method, ...) \
    spdk_flint::Logger::get()->debug("[RPC] SPDK method {} succeeded " __VA_ARGS__, method)

#define LOG_RPC_ERROR(method, error, ...) \
    spdk_flint::Logger::get()->error("[RPC] SPDK method {} failed: {} " __VA_ARGS__, method, error)

// SPDK-specific logging macros
#define LOG_SPDK_WARN(...) spdk_flint::Logger::get()->warn(__VA_ARGS__)
#define LOG_SPDK_ERROR(...) spdk_flint::Logger::get()->error(__VA_ARGS__)

} // namespace spdk_flint

#endif // SPDK_FLINT_LOGGING_HPP 
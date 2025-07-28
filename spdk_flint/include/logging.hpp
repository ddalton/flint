#pragma once

#include <spdlog/spdlog.h>
#include <spdlog/sinks/stdout_color_sinks.h>
#include <spdlog/sinks/rotating_file_sink.h>
#include <spdlog/fmt/fmt.h>
#include <memory>
#include <string>

namespace spdk_flint {

class Logger {
public:
    static void initialize(const std::string& name, const std::string& level = "info");
    static std::shared_ptr<spdlog::logger> get();
    static void setLevel(const std::string& level);
    static void shutdown();

private:
    static std::shared_ptr<spdlog::logger> logger_;
    static bool initialized_;
};

// Simple macro-based logging that works reliably
#define LOG_TRACE(...) ::spdk_flint::Logger::get()->trace(__VA_ARGS__)
#define LOG_DEBUG(...) ::spdk_flint::Logger::get()->debug(__VA_ARGS__)
#define LOG_INFO(...) ::spdk_flint::Logger::get()->info(__VA_ARGS__)
#define LOG_WARN(...) ::spdk_flint::Logger::get()->warn(__VA_ARGS__)
#define LOG_ERROR(...) ::spdk_flint::Logger::get()->error(__VA_ARGS__)
#define LOG_CRITICAL(...) ::spdk_flint::Logger::get()->critical(__VA_ARGS__)

#define LOG_CSI_INFO(component, fmt, ...) ::spdk_flint::Logger::get()->info("[CSI] " fmt, ##__VA_ARGS__)
#define LOG_CSI_ERROR(component, fmt, ...) ::spdk_flint::Logger::get()->error("[CSI] " fmt, ##__VA_ARGS__)

#define LOG_CONTROLLER_INFO(fmt, ...) ::spdk_flint::Logger::get()->info("[CONTROLLER] " fmt, ##__VA_ARGS__)
#define LOG_CONTROLLER_ERROR(fmt, ...) ::spdk_flint::Logger::get()->error("[CONTROLLER] " fmt, ##__VA_ARGS__)
#define LOG_CONTROLLER_DEBUG(fmt, ...) ::spdk_flint::Logger::get()->debug("[CONTROLLER] " fmt, ##__VA_ARGS__)

#define LOG_SPDK_INFO(fmt, ...) ::spdk_flint::Logger::get()->info("[SPDK] " fmt, ##__VA_ARGS__)
#define LOG_SPDK_WARN(fmt, ...) ::spdk_flint::Logger::get()->warn("[SPDK] " fmt, ##__VA_ARGS__)
#define LOG_SPDK_ERROR(fmt, ...) ::spdk_flint::Logger::get()->error("[SPDK] " fmt, ##__VA_ARGS__)

#define LOG_DASHBOARD_INFO(fmt, ...) ::spdk_flint::Logger::get()->info("[DASHBOARD] " fmt, ##__VA_ARGS__)
#define LOG_DASHBOARD_ERROR(fmt, ...) ::spdk_flint::Logger::get()->error("[DASHBOARD] " fmt, ##__VA_ARGS__)

#define LOG_NODE_AGENT_INFO(fmt, ...) ::spdk_flint::Logger::get()->info("[NODE_AGENT] " fmt, ##__VA_ARGS__)
#define LOG_NODE_AGENT_DEBUG(fmt, ...) ::spdk_flint::Logger::get()->debug("[NODE_AGENT] " fmt, ##__VA_ARGS__)
#define LOG_NODE_AGENT_ERROR(fmt, ...) ::spdk_flint::Logger::get()->error("[NODE_AGENT] " fmt, ##__VA_ARGS__)

#define LOG_RPC_CALL(fmt, ...) ::spdk_flint::Logger::get()->debug("[RPC] " fmt, ##__VA_ARGS__)
#define LOG_RPC_SUCCESS(...) ::spdk_flint::Logger::get()->debug(__VA_ARGS__)
#define LOG_RPC_ERROR(...) ::spdk_flint::Logger::get()->error(__VA_ARGS__)

} // namespace spdk_flint 
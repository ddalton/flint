#pragma once

#include <spdlog/spdlog.h>
#include <spdlog/sinks/stdout_color_sinks.h>
#include <spdlog/sinks/rotating_file_sink.h>
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

// Core logging functions
template<typename... Args>
inline void LOG_TRACE(Args&&... args) {
    ::spdk_flint::Logger::get()->trace(std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_DEBUG(Args&&... args) {
    ::spdk_flint::Logger::get()->debug(std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_INFO(Args&&... args) {
    ::spdk_flint::Logger::get()->info(std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_WARN(Args&&... args) {
    ::spdk_flint::Logger::get()->warn(std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_ERROR(Args&&... args) {
    ::spdk_flint::Logger::get()->error(std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_CRITICAL(Args&&... args) {
    ::spdk_flint::Logger::get()->critical(std::forward<Args>(args)...);
}

// Component-specific logging functions
template<typename... Args>
inline void LOG_CSI_INFO(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->info("[CSI] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_CSI_ERROR(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->error("[CSI] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_CONTROLLER_INFO(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->info("[CONTROLLER] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_CONTROLLER_ERROR(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->error("[CONTROLLER] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_CONTROLLER_DEBUG(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->debug("[CONTROLLER] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_SPDK_INFO(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->info("[SPDK] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_SPDK_WARN(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->warn("[SPDK] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_SPDK_ERROR(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->error("[SPDK] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_DASHBOARD_INFO(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->info("[DASHBOARD] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_DASHBOARD_ERROR(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->error("[DASHBOARD] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_NODE_AGENT_INFO(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->info("[NODE_AGENT] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_NODE_AGENT_DEBUG(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->debug("[NODE_AGENT] " + format, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_NODE_AGENT_ERROR(const std::string& format, Args&&... args) {
    ::spdk_flint::Logger::get()->error("[NODE_AGENT] " + format, std::forward<Args>(args)...);
}

// RPC operation logging functions
template<typename... Args>
inline void LOG_RPC_CALL(const std::string& method, Args&&... args) {
    ::spdk_flint::Logger::get()->debug("[RPC] Calling SPDK method: " + method, std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_RPC_SUCCESS(Args&&... args) {
    ::spdk_flint::Logger::get()->debug(std::forward<Args>(args)...);
}

template<typename... Args>
inline void LOG_RPC_ERROR(Args&&... args) {
    ::spdk_flint::Logger::get()->error(std::forward<Args>(args)...);
}

} // namespace spdk_flint 
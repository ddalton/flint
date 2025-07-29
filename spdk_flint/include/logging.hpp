#pragma once

#include <spdlog/spdlog.h>
#include <spdlog/sinks/stdout_color_sinks.h>
#include <spdlog/sinks/rotating_file_sink.h>
#include <spdlog/fmt/fmt.h>
#include <memory>
#include <string>
#include <thread>
#include <sstream>

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

// Simple, direct spdlog access - no macros, no complexity
inline auto logger() { return Logger::get(); }

// Utility function to format thread IDs for logging
inline std::string thread_id_to_string(std::thread::id id) {
    std::ostringstream oss;
    oss << id;
    return oss.str();
}

// Convenience function to get current thread ID as string
inline std::string current_thread_id() {
    return thread_id_to_string(std::this_thread::get_id());
}

} // namespace spdk_flint 
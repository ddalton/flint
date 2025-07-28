#include "logging.hpp"
#include <iostream>
#include <mutex>

namespace spdk_flint {

std::shared_ptr<spdlog::logger> Logger::logger_;
bool Logger::initialized_ = false;

void Logger::initialize(const std::string& name, const std::string& level) {
    static std::mutex init_mutex;
    std::lock_guard<std::mutex> lock(init_mutex);
    
    if (initialized_) {
        return;
    }
    
    try {
        // Create console logger with colors
        auto console_sink = std::make_shared<spdlog::sinks::stdout_color_sink_mt>();
        console_sink->set_pattern("[%Y-%m-%d %H:%M:%S.%e] [%l] [%t] %v");
        
        // Create logger
        logger_ = std::make_shared<spdlog::logger>(name, console_sink);
        
        // Set log level
        setLevel(level);
        
        // Set flush policy
        logger_->flush_on(spdlog::level::warn);
        spdlog::flush_every(std::chrono::seconds(5));
        
        // Register as default logger
        spdlog::set_default_logger(logger_);
        
        initialized_ = true;
        
        logger_->info("Logging system initialized with level: {}", level);
        
    } catch (const spdlog::spdlog_ex& ex) {
        std::cerr << "Log initialization failed: " << ex.what() << std::endl;
        throw;
    }
}

std::shared_ptr<spdlog::logger> Logger::get() {
    if (!initialized_) {
        initialize("spdk_flint", "info");
    }
    
    return logger_;
}

void Logger::setLevel(const std::string& level) {
    if (!logger_) {
        return;
    }
    
    spdlog::level::level_enum log_level = spdlog::level::info;
    
    if (level == "trace") {
        log_level = spdlog::level::trace;
    } else if (level == "debug") {
        log_level = spdlog::level::debug;
    } else if (level == "info") {
        log_level = spdlog::level::info;
    } else if (level == "warn" || level == "warning") {
        log_level = spdlog::level::warn;
    } else if (level == "error") {
        log_level = spdlog::level::err;
    } else if (level == "critical") {
        log_level = spdlog::level::critical;
    } else {
        logger_->warn("Unknown log level '{}', using 'info'", level);
        log_level = spdlog::level::info;
    }
    
    logger_->set_level(log_level);
    spdlog::set_level(log_level);
}

void Logger::shutdown() {
    if (initialized_) {
        if (logger_) {
            logger_->info("Shutting down logging system");
        }
        spdlog::shutdown();
        logger_.reset();
        initialized_ = false;
    }
}

} // namespace spdk_flint 
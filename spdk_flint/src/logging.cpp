#include "logging.hpp"
#include <iostream>
#include <mutex>

namespace spdk_flint {

std::shared_ptr<spdlog::logger> Logger::default_logger_;
bool Logger::initialized_ = false;

void Logger::initialize(const std::string& level, 
                       const std::string& log_file,
                       size_t max_file_size,
                       size_t max_files) {
    static std::mutex init_mutex;
    std::lock_guard<std::mutex> lock(init_mutex);
    
    if (initialized_) {
        return;
    }
    
    try {
        std::vector<spdlog::sink_ptr> sinks;
        
        // Always add console sink with colors
        auto console_sink = std::make_shared<spdlog::sinks::stdout_color_sink_mt>();
        console_sink->set_pattern("[%Y-%m-%d %H:%M:%S.%e] [%l] [%t] %v");
        sinks.push_back(console_sink);
        
        // Add file sink if specified
        if (!log_file.empty()) {
            auto file_sink = std::make_shared<spdlog::sinks::rotating_file_sink_mt>(
                log_file, max_file_size, max_files);
            file_sink->set_pattern("[%Y-%m-%d %H:%M:%S.%e] [%l] [%t] %v");
            sinks.push_back(file_sink);
        }
        
        // Create multi-sink logger
        default_logger_ = std::make_shared<spdlog::logger>("spdk_flint", 
                                                          sinks.begin(), 
                                                          sinks.end());
        
        // Set log level
        setLevel(level);
        
        // Set flush policy
        default_logger_->flush_on(spdlog::level::warn);
        spdlog::flush_every(std::chrono::seconds(5));
        
        // Register as default logger
        spdlog::set_default_logger(default_logger_);
        
        initialized_ = true;
        
        default_logger_->info("Logging system initialized with level: {}", level);
        
    } catch (const spdlog::spdlog_ex& ex) {
        std::cerr << "Log initialization failed: " << ex.what() << std::endl;
        throw;
    }
}

std::shared_ptr<spdlog::logger> Logger::get(const std::string& name) {
    if (!initialized_) {
        initialize();
    }
    
    if (name == "spdk_flint" || name.empty()) {
        return default_logger_;
    }
    
    // Return existing logger or create new one
    auto logger = spdlog::get(name);
    if (logger) {
        return logger;
    }
    
    // Create new logger with same sinks as default
    if (default_logger_) {
        auto new_logger = std::make_shared<spdlog::logger>(name, 
                                                          default_logger_->sinks().begin(),
                                                          default_logger_->sinks().end());
        new_logger->set_level(default_logger_->level());
        spdlog::register_logger(new_logger);
        return new_logger;
    }
    
    return default_logger_;
}

void Logger::setLevel(const std::string& level) {
    if (!default_logger_) {
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
        default_logger_->warn("Unknown log level '{}', using 'info'", level);
        log_level = spdlog::level::info;
    }
    
    default_logger_->set_level(log_level);
    spdlog::set_level(log_level);
}

void Logger::shutdown() {
    if (initialized_) {
        if (default_logger_) {
            default_logger_->info("Shutting down logging system");
        }
        spdlog::shutdown();
        default_logger_.reset();
        initialized_ = false;
    }
}

} // namespace spdk_flint 
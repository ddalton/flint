#include "logging.hpp"
#include <iostream>
#include <mutex>
#include <chrono>
#include <iomanip>
#include <sstream>

namespace spdk_flint {

std::shared_ptr<spdlog::logger> Logger::logger_;
bool Logger::initialized_ = false;

void Logger::initialize(const std::string& name, const std::string& level) {
    static std::mutex init_mutex;
    std::lock_guard<std::mutex> lock(init_mutex);
    
    // Print early startup logging to console
    auto now = std::chrono::system_clock::now();
    auto time_t = std::chrono::system_clock::to_time_t(now);
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
        now.time_since_epoch()) % 1000;
    
    std::cout << "[" << std::put_time(std::localtime(&time_t), "%Y-%m-%d %H:%M:%S") 
              << "." << std::setfill('0') << std::setw(3) << ms.count() << "] "
              << "[INFO] [LOGGING] Initializing logging system: name='" << name 
              << "', level='" << level << "'" << std::endl;
    
    if (initialized_) {
        std::cout << "[" << std::put_time(std::localtime(&time_t), "%Y-%m-%d %H:%M:%S") 
                  << "." << std::setfill('0') << std::setw(3) << ms.count() << "] "
                  << "[WARN] [LOGGING] Logging already initialized - skipping duplicate initialization" << std::endl;
        return;
    }
    
    try {
        // Create console logger with colors and detailed formatting
        auto console_sink = std::make_shared<spdlog::sinks::stdout_color_sink_mt>();
        
        // Enhanced log pattern with thread ID and microsecond precision
        console_sink->set_pattern("[%Y-%m-%d %H:%M:%S.%f] [%^%l%$] [thread %t] %v");
        
        // Set color mode
        console_sink->set_color_mode(spdlog::color_mode::automatic);
        
        // Create logger with the sink
        logger_ = std::make_shared<spdlog::logger>(name, console_sink);
        
        // Configure logger behavior
        logger_->set_error_handler([](const std::string& msg) {
            std::cerr << "[LOGGING ERROR] " << msg << std::endl;
        });
        
        // Set log level first
        setLevel(level);
        
        // Configure flush behavior for different log levels
        logger_->flush_on(spdlog::level::warn);  // Auto-flush on warnings and errors
        spdlog::flush_every(std::chrono::seconds(3));  // Periodic flush every 3 seconds
        
        // Register as default logger for spdlog
        spdlog::set_default_logger(logger_);
        
        // Set global error handler
        spdlog::set_error_handler([](const std::string& msg) {
            std::cerr << "[SPDLOG ERROR] " << msg << std::endl;
        });
        
        initialized_ = true;
        
        // Log successful initialization
        logger_->info("[LOGGING] Logging system initialized successfully");
        logger_->info("[LOGGING] Logger name: '{}', level: '{}'", name, level);
        logger_->debug("[LOGGING] Log pattern: [timestamp] [level] [thread] message");
        logger_->debug("[LOGGING] Auto-flush: warnings and above, periodic flush: 3 seconds");
        
    } catch (const spdlog::spdlog_ex& ex) {
        std::cerr << "[LOGGING CRITICAL] Log initialization failed: " << ex.what() << std::endl;
        std::cerr << "[LOGGING CRITICAL] Logger name: '" << name << "', level: '" << level << "'" << std::endl;
        std::cerr << "[LOGGING CRITICAL] This is a fatal error - logging will not work" << std::endl;
        throw;
    } catch (const std::exception& ex) {
        std::cerr << "[LOGGING CRITICAL] Unexpected error during log initialization: " << ex.what() << std::endl;
        throw;
    }
}

std::shared_ptr<spdlog::logger> Logger::get() {
    if (!initialized_) {
        // Auto-initialize with default settings if not already initialized
        std::cout << "[WARN] Logger not initialized, auto-initializing with defaults" << std::endl;
        initialize("spdk_flint_auto", "info");
    }
    
    if (!logger_) {
        // This should never happen if initialization succeeded
        std::cerr << "[CRITICAL] Logger is null even after initialization!" << std::endl;
        throw std::runtime_error("Logger is null after initialization");
    }
    
    return logger_;
}

void Logger::setLevel(const std::string& level) {
    if (!logger_) {
        std::cerr << "[LOGGING ERROR] Cannot set log level: logger is null" << std::endl;
        return;
    }
    
    spdlog::level::level_enum log_level = spdlog::level::info;
    bool valid_level = true;
    
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
        valid_level = false;
        log_level = spdlog::level::info;
    }
    
    try {
        logger_->set_level(log_level);
        spdlog::set_level(log_level);
        
        if (valid_level) {
            logger_->debug("[LOGGING] Log level set to: {}", level);
        } else {
            logger_->warn("[LOGGING] Unknown log level '{}', defaulted to 'info'", level);
            logger_->debug("[LOGGING] Valid levels: trace, debug, info, warn/warning, error, critical");
        }
    } catch (const std::exception& ex) {
        std::cerr << "[LOGGING ERROR] Failed to set log level '" << level << "': " << ex.what() << std::endl;
    }
}

void Logger::shutdown() {
    static std::mutex shutdown_mutex;
    std::lock_guard<std::mutex> lock(shutdown_mutex);
    
    if (!initialized_) {
        return;  // Already shut down or never initialized
    }
    
    try {
        if (logger_) {
            logger_->info("[LOGGING] Shutting down logging system");
            logger_->debug("[LOGGING] Flushing all pending log messages");
            logger_->flush();
        }
        
        // Shutdown spdlog and flush all loggers
        spdlog::shutdown();
        
        // Reset our logger reference
        logger_.reset();
        initialized_ = false;
        
        // Print final message to console since logger is gone
        auto now = std::chrono::system_clock::now();
        auto time_t = std::chrono::system_clock::to_time_t(now);
        auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(
            now.time_since_epoch()) % 1000;
        
        std::cout << "[" << std::put_time(std::localtime(&time_t), "%Y-%m-%d %H:%M:%S") 
                  << "." << std::setfill('0') << std::setw(3) << ms.count() << "] "
                  << "[INFO] [LOGGING] Logging system shutdown complete" << std::endl;
                  
    } catch (const std::exception& ex) {
        std::cerr << "[LOGGING ERROR] Error during logging shutdown: " << ex.what() << std::endl;
        // Continue shutdown despite errors
        logger_.reset();
        initialized_ = false;
    }
}

} // namespace spdk_flint 
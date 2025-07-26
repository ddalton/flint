import React, { createContext, useContext, useState, useRef, useEffect } from 'react';
import type { ReactNode } from 'react';

interface OperationsContextType {
  hasActiveOperations: boolean;
  hasActiveSelections: boolean;
  shouldPauseRefresh: boolean;
  setActiveOperationsCount: (count: number) => void;
  setActiveSelectionsCount: (count: number) => void;
  incrementOperations: () => void;
  decrementOperations: () => void;
}

const OperationsContext = createContext<OperationsContextType | undefined>(undefined);

export const useOperations = () => {
  const context = useContext(OperationsContext);
  if (context === undefined) {
    throw new Error('useOperations must be used within an OperationsProvider');
  }
  return context;
};

interface OperationsProviderProps {
  children: ReactNode;
  autoRefresh?: boolean;
  onAutoRefreshChange?: (enabled: boolean) => void;
}

export const OperationsProvider: React.FC<OperationsProviderProps> = ({ 
  children, 
  autoRefresh, 
  onAutoRefreshChange 
}) => {
  const [activeOperationsCount, setActiveOperationsCount] = useState(0);
  const [activeSelectionsCount, setActiveSelectionsCount] = useState(0);
  
  // Track whether auto-refresh was disabled manually by user vs automatically by system
  const [wasAutoDisabledBySelections, setWasAutoDisabledBySelections] = useState(false);
  const prevSelectionsCount = useRef(0);

  const hasActiveOperations = activeOperationsCount > 0;
  const hasActiveSelections = activeSelectionsCount > 0;
  const shouldPauseRefresh = hasActiveOperations || hasActiveSelections;
  
  // Auto-manage refresh based on selections (only if auto-refresh management is enabled)
  useEffect(() => {
    if (!onAutoRefreshChange) return; // Don't auto-manage if no callback provided
    
    const currentSelectionsCount = activeSelectionsCount;
    const previousSelectionsCount = prevSelectionsCount.current;
    
    // Selections started (0 → >0): Auto-disable refresh
    if (previousSelectionsCount === 0 && currentSelectionsCount > 0 && autoRefresh) {
      console.log('🔄 [AUTO_REFRESH_MANAGER] Auto-disabling refresh due to selections');
      setWasAutoDisabledBySelections(true);
      onAutoRefreshChange(false);
    }
    
    // Selections ended (>0 → 0): Auto-enable refresh only if it was auto-disabled
    if (previousSelectionsCount > 0 && currentSelectionsCount === 0 && wasAutoDisabledBySelections) {
      console.log('🔄 [AUTO_REFRESH_MANAGER] Auto-enabling refresh after selections cleared');
      setWasAutoDisabledBySelections(false);
      onAutoRefreshChange(true);
    }
    
    prevSelectionsCount.current = currentSelectionsCount;
  }, [activeSelectionsCount, autoRefresh, onAutoRefreshChange, wasAutoDisabledBySelections]);
  
  // Reset auto-disable flag if user manually enables refresh
  useEffect(() => {
    if (autoRefresh && wasAutoDisabledBySelections && activeSelectionsCount === 0) {
      console.log('🔄 [AUTO_REFRESH_MANAGER] User manually enabled refresh, clearing auto-disable flag');
      setWasAutoDisabledBySelections(false);
    }
  }, [autoRefresh, wasAutoDisabledBySelections, activeSelectionsCount]);
  
  // Debug when pause state changes
  const prevShouldPause = useRef(shouldPauseRefresh);
  if (prevShouldPause.current !== shouldPauseRefresh) {
    console.log(`🔄 [CONTEXT_STATE_CHANGE] shouldPauseRefresh changed: ${prevShouldPause.current} → ${shouldPauseRefresh} (ops: ${activeOperationsCount}, selections: ${activeSelectionsCount})`);
    prevShouldPause.current = shouldPauseRefresh;
  }

  const incrementOperations = () => {
    setActiveOperationsCount(prev => prev + 1);
  };

  const decrementOperations = () => {
    setActiveOperationsCount(prev => Math.max(0, prev - 1));
  };

  const setActiveOperationsCountDirect = (count: number) => {
    setActiveOperationsCount(Math.max(0, count));
  };

  const setActiveSelectionsCountDirect = (count: number) => {
    console.log(`🔄 [CONTEXT_UPDATE] setActiveSelectionsCount called with: ${count}`);
    const newCount = Math.max(0, count);
    console.log(`🔄 [CONTEXT_UPDATE] Setting activeSelectionsCount to: ${newCount}`);
    setActiveSelectionsCount(newCount);
  };

  return (
    <OperationsContext.Provider value={{
      hasActiveOperations,
      hasActiveSelections,
      shouldPauseRefresh,
      setActiveOperationsCount: setActiveOperationsCountDirect,
      setActiveSelectionsCount: setActiveSelectionsCountDirect,
      incrementOperations,
      decrementOperations,
    }}>
      {children}
    </OperationsContext.Provider>
  );
}; 
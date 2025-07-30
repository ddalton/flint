import React, { createContext, useContext, useState, useRef, useEffect } from 'react';
import type { ReactNode } from 'react';

interface OperationsContextType {
  hasActiveOperations: boolean;
  hasActiveSelections: boolean;
  isDialogVisible: boolean;
  shouldPauseRefresh: boolean;
  setActiveOperationsCount: (count: number) => void;
  setActiveSelectionsCount: (count: number) => void;
  setDialogVisible: (visible: boolean) => void;
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
  const [isDialogVisible, setDialogVisible] = useState(false);
  
  // Track whether auto-refresh was disabled manually by user vs automatically by system
  const [wasAutoDisabled, setWasAutoDisabled] = useState(false);
  const prevSelectionsCount = useRef(0);
  const prevDialogVisible = useRef(false);

  const hasActiveOperations = activeOperationsCount > 0;
  const hasActiveSelections = activeSelectionsCount > 0;
  const shouldPauseRefresh = hasActiveOperations || hasActiveSelections || isDialogVisible;
  
  // Auto-manage refresh based on selections or dialog visibility
  useEffect(() => {
    if (!onAutoRefreshChange) return; // Don't auto-manage if no callback provided
    
    const selectionsStarted = prevSelectionsCount.current === 0 && activeSelectionsCount > 0;
    const dialogOpened = !prevDialogVisible.current && isDialogVisible;

    // Pause on new selection or dialog
    if ((selectionsStarted || dialogOpened) && autoRefresh) {
      const reason = selectionsStarted ? 'selections' : 'dialog';
      console.log(`🔄 [AUTO_REFRESH_MANAGER] Auto-disabling refresh due to ${reason}`);
      setWasAutoDisabled(true);
      onAutoRefreshChange(false);
    }
    
    const selectionsEnded = prevSelectionsCount.current > 0 && activeSelectionsCount === 0;
    const dialogClosed = prevDialogVisible.current && !isDialogVisible;

    // Resume only when all conditions are clear
    if ((selectionsEnded || dialogClosed) && wasAutoDisabled && activeSelectionsCount === 0 && !isDialogVisible) {
      console.log('🔄 [AUTO_REFRESH_MANAGER] Auto-enabling refresh');
      setWasAutoDisabled(false);
      onAutoRefreshChange(true);
    }
    
    prevSelectionsCount.current = activeSelectionsCount;
    prevDialogVisible.current = isDialogVisible;
  }, [activeSelectionsCount, isDialogVisible, autoRefresh, onAutoRefreshChange, wasAutoDisabled]);
  
  // Reset auto-disable flag if user manually enables refresh
  useEffect(() => {
    if (autoRefresh && wasAutoDisabled && activeSelectionsCount === 0 && !isDialogVisible) {
      console.log('🔄 [AUTO_REFRESH_MANAGER] User manually enabled refresh, clearing auto-disable flag');
      setWasAutoDisabled(false);
    }
  }, [autoRefresh, wasAutoDisabled, activeSelectionsCount, isDialogVisible]);
  
  // Debug when pause state changes
  const prevShouldPause = useRef(shouldPauseRefresh);
  if (prevShouldPause.current !== shouldPauseRefresh) {
    console.log(`🔄 [CONTEXT_STATE_CHANGE] shouldPauseRefresh changed: ${prevShouldPause.current} → ${shouldPauseRefresh} (ops: ${activeOperationsCount}, selections: ${activeSelectionsCount}, dialog: ${isDialogVisible})`);
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
    const newCount = Math.max(0, count);
    setActiveSelectionsCount(newCount);
  };

  const setDialogVisibleDirect = (visible: boolean) => {
    setDialogVisible(visible);
  };

  return (
    <OperationsContext.Provider value={{
      hasActiveOperations,
      hasActiveSelections,
      isDialogVisible,
      shouldPauseRefresh,
      setActiveOperationsCount: setActiveOperationsCountDirect,
      setActiveSelectionsCount: setActiveSelectionsCountDirect,
      setDialogVisible: setDialogVisibleDirect,
      incrementOperations,
      decrementOperations,
    }}>
      {children}
    </OperationsContext.Provider>
  );
};
import React, { createContext, useContext, useState, useRef } from 'react';
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
}

export const OperationsProvider: React.FC<OperationsProviderProps> = ({ children }) => {
  const [activeOperationsCount, setActiveOperationsCount] = useState(0);
  const [activeSelectionsCount, setActiveSelectionsCount] = useState(0);

  const hasActiveOperations = activeOperationsCount > 0;
  const hasActiveSelections = activeSelectionsCount > 0;
  const shouldPauseRefresh = hasActiveOperations || hasActiveSelections;
  
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
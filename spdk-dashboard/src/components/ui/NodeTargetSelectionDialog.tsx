import React, { useState, useEffect } from 'react';
import { Server, ArrowRight, AlertTriangle, Info } from 'lucide-react';

interface NodeTargetSelectionDialogProps {
  isOpen: boolean;
  onClose: () => void;
  onConfirm: (targetNode?: string) => void;
  title: string;
  description: string;
  confirmText: string;
  availableNodes: string[];
  currentNode?: string;
  warningMessage?: string;
  infoMessage?: string;
  isLoading?: boolean;
}

export const NodeTargetSelectionDialog: React.FC<NodeTargetSelectionDialogProps> = ({
  isOpen,
  onClose,
  onConfirm,
  title,
  description,
  confirmText,
  availableNodes,
  currentNode,
  warningMessage,
  infoMessage,
  isLoading = false
}) => {
  const [selectedTargetNode, setSelectedTargetNode] = useState<string>('auto');
  const [isConfirming, setIsConfirming] = useState(false);

  // Filter out current node from available targets
  const validTargetNodes = availableNodes.filter(node => node !== currentNode);

  useEffect(() => {
    if (isOpen) {
      setSelectedTargetNode('auto');
      setIsConfirming(false);
    }
  }, [isOpen]);

  const handleConfirm = async () => {
    setIsConfirming(true);
    try {
      const targetNode = selectedTargetNode === 'auto' ? undefined : selectedTargetNode;
      await onConfirm(targetNode);
    } finally {
      setIsConfirming(false);
    }
  };

  if (!isOpen) return null;

  return (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
      <div className="bg-white rounded-lg shadow-xl max-w-md w-full mx-4">
        {/* Header */}
        <div className="flex items-center justify-between p-6 border-b">
          <div className="flex items-center gap-3">
            <Server className="w-6 h-6 text-blue-600" />
            <h2 className="text-lg font-semibold">{title}</h2>
          </div>
          <button
            onClick={onClose}
            disabled={isConfirming}
            className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md disabled:opacity-50"
          >
            ×
          </button>
        </div>

        {/* Content */}
        <div className="p-6 space-y-4">
          <p className="text-gray-700">{description}</p>

          {/* Current Node Info */}
          {currentNode && (
            <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
              <div className="flex items-center gap-2">
                <Server className="w-4 h-4 text-blue-600" />
                <span className="text-sm font-medium text-blue-900">Current Node:</span>
                <span className="text-sm text-blue-700">{currentNode}</span>
              </div>
            </div>
          )}

          {/* Warning Message */}
          {warningMessage && (
            <div className="bg-yellow-50 border border-yellow-200 rounded-lg p-3">
              <div className="flex items-start gap-2">
                <AlertTriangle className="w-4 h-4 text-yellow-600 mt-0.5 flex-shrink-0" />
                <p className="text-sm text-yellow-800">{warningMessage}</p>
              </div>
            </div>
          )}

          {/* Info Message */}
          {infoMessage && (
            <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
              <div className="flex items-start gap-2">
                <Info className="w-4 h-4 text-blue-600 mt-0.5 flex-shrink-0" />
                <p className="text-sm text-blue-800">{infoMessage}</p>
              </div>
            </div>
          )}

          {/* Target Node Selection */}
          <div className="space-y-3">
            <label className="block text-sm font-medium text-gray-700">
              Target Node Selection
            </label>
            
            <div className="space-y-2">
              {/* Auto Selection Option */}
              <label className="flex items-center gap-3 p-3 border rounded-lg cursor-pointer hover:bg-gray-50">
                <input
                  type="radio"
                  name="targetNode"
                  value="auto"
                  checked={selectedTargetNode === 'auto'}
                  onChange={(e) => setSelectedTargetNode(e.target.value)}
                  className="text-blue-600"
                />
                <div className="flex-1">
                  <div className="flex items-center gap-2">
                    <span className="font-medium">Automatic Selection</span>
                    <span className="text-xs bg-green-100 text-green-800 px-2 py-1 rounded-full">
                      Recommended
                    </span>
                  </div>
                  <p className="text-sm text-gray-600 mt-1">
                    Let the system intelligently choose the best target node based on capacity, performance, and current load.
                  </p>
                </div>
              </label>

              {/* Manual Selection */}
              {validTargetNodes.length > 0 && (
                <div className="border rounded-lg">
                  <div className="p-3 border-b bg-gray-50">
                    <span className="text-sm font-medium text-gray-700">Manual Selection</span>
                  </div>
                  <div className="p-3 space-y-2">
                    {validTargetNodes.map((node) => (
                      <label key={node} className="flex items-center gap-3 p-2 hover:bg-gray-50 rounded cursor-pointer">
                        <input
                          type="radio"
                          name="targetNode"
                          value={node}
                          checked={selectedTargetNode === node}
                          onChange={(e) => setSelectedTargetNode(e.target.value)}
                          className="text-blue-600"
                        />
                        <div className="flex items-center gap-2">
                          <Server className="w-4 h-4 text-gray-500" />
                          <span className="font-medium">{node}</span>
                        </div>
                      </label>
                    ))}
                  </div>
                </div>
              )}

              {/* No available targets warning */}
              {validTargetNodes.length === 0 && (
                <div className="bg-yellow-50 border border-yellow-200 rounded-lg p-3">
                  <div className="flex items-center gap-2">
                    <AlertTriangle className="w-4 h-4 text-yellow-600" />
                    <span className="text-sm text-yellow-800">
                      No other nodes available for manual selection. Automatic selection will be used.
                    </span>
                  </div>
                </div>
              )}
            </div>
          </div>

          {/* Selection Preview */}
          {selectedTargetNode !== 'auto' && (
            <div className="bg-gray-50 border border-gray-200 rounded-lg p-3">
              <div className="flex items-center gap-2 text-sm">
                <span className="text-gray-600">Target:</span>
                <span className="font-medium">{currentNode}</span>
                <ArrowRight className="w-4 h-4 text-gray-400" />
                <span className="font-medium text-blue-600">{selectedTargetNode}</span>
              </div>
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex justify-end gap-3 p-6 border-t bg-gray-50">
          <button
            onClick={onClose}
            disabled={isConfirming}
            className="px-4 py-2 text-gray-700 border border-gray-300 rounded-lg hover:bg-gray-100 disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            onClick={handleConfirm}
            disabled={isConfirming || isLoading}
            className="px-4 py-2 bg-blue-600 text-white rounded-lg hover:bg-blue-700 disabled:opacity-50 flex items-center gap-2"
          >
            {isConfirming ? (
              <>
                <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                <span>Processing...</span>
              </>
            ) : (
              confirmText
            )}
          </button>
        </div>
      </div>
    </div>
  );
};



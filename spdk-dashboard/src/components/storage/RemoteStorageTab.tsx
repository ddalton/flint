import React, { useState, useEffect } from 'react';
// Deprecated component retained temporarily (no longer used in UI) – consider removal later
import { useOperations } from '../../contexts/OperationsContext';
import { 
  WifiOff, CheckCircle, AlertTriangle, X, Wifi, Edit3, Trash2, 
  EyeOff, Eye, Loader2, Search, Save, Plus, RefreshCw, Network, HardDrive 
} from 'lucide-react';

// Types for remote storage targets
interface NVMeOFTarget {
  id: string;
  name: string;
  nqn: string;
  transport: 'tcp' | 'rdma' | 'fc';
  address: string;
  port: string;
  nsid: string; // Namespace ID is required
  connected: boolean;
  status: 'healthy' | 'degraded' | 'failed';
  capacity?: string;
  lastConnected?: string;
}

interface iSCSITarget {
  id: string;
  name: string;
  targetIQN: string;
  portalIP: string;
  port: string;
  lun: number;
  connected: boolean;
  status: 'healthy' | 'degraded' | 'failed';
  capacity?: string;
  lastConnected?: string;
  authMethod?: 'none' | 'chap';
  username?: string;
  password?: string;
}

type StorageTargetType = 'nvmeof' | 'iscsi';

// Mock data for discovered namespaces
const mockDiscoveredNamespaces = [
    { nsid: 1, size: '100 GiB', status: 'Healthy', attached: false },
    { nsid: 2, size: '250 GiB', status: 'Healthy', attached: true },
];

const RemoteStorageTab: React.FC = () => {
  const { setDialogVisible } = useOperations();
  
  // State for NVMe-oF targets
  const [nvmeTargets, setNvmeTargets] = useState<NVMeOFTarget[]>([
    {
      id: '1',
      name: 'Production Storage Array 1',
      nqn: 'nqn.2023.io.storage:array1.target1',
      transport: 'tcp',
      address: '192.168.1.100',
      port: '4420',
      nsid: '1',
      connected: true,
      status: 'healthy',
      capacity: '2.5TB',
      lastConnected: '2024-01-15 14:30:00'
    }
  ]);

  // State for iSCSI targets
  const [iscsiTargets, setIscsiTargets] = useState<iSCSITarget[]>([
    {
      id: '1',
      name: 'Legacy SAN Storage',
      targetIQN: 'iqn.2023.io.san:storage.target1',
      portalIP: '192.168.1.201',
      port: '3260',
      lun: 1,
      connected: true,
      status: 'healthy',
      capacity: '5.0TB',
      lastConnected: '2024-01-15 14:25:00',
      authMethod: 'chap',
      username: 'storage_user'
    }
  ]);

  // UI State
  const [activeTab, setActiveTab] = useState<StorageTargetType>('nvmeof');
  const [showAddForm, setShowAddForm] = useState(false);
  const [editingTarget, setEditingTarget] = useState<string | null>(null);
  const [showPasswords, setShowPasswords] = useState<{[key: string]: boolean}>({});

  // Form state for new/editing targets
  const [formData, setFormData] = useState<Partial<NVMeOFTarget & iSCSITarget>>({});
  
  // State for the NVMe-oF discovery process
  const [isDiscovering, setIsDiscovering] = useState(false);
  const [discoveredNamespaces, setDiscoveredNamespaces] = useState<any[]>([]);
  const [selectedNamespaceId, setSelectedNamespaceId] = useState<string | null>(null);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);

  // Inform context about dialog visibility
  useEffect(() => {
    setDialogVisible(showAddForm);
  }, [showAddForm, setDialogVisible]);
  
  // Reset form defaults when tab changes
  useEffect(() => {
    resetForm(true); // soft reset
  }, [activeTab]);

  const handleFormChange = (field: string, value: any) => {
    const newFormData = { ...formData, [field]: value };
    setFormData(newFormData);

    // If core connection details change for an NVMe-oF target, reset discovery state
    if (activeTab === 'nvmeof' && ['nqn', 'address', 'port'].includes(field)) {
        setSelectedNamespaceId(null);
        setDiscoveredNamespaces([]);
        setDiscoveryError(null);
    }
  };


  // Mock functions - replace with actual API calls
  const connectTarget = async (id: string, type: StorageTargetType) => {
    console.log(`Connecting ${type} target ${id}`);
    await new Promise(resolve => setTimeout(resolve, 1000));
    if (type === 'nvmeof') {
      setNvmeTargets(prev => prev.map(target => 
        target.id === id ? { ...target, connected: true, status: 'healthy' } : target
      ));
    } else {
      setIscsiTargets(prev => prev.map(target => 
        target.id === id ? { ...target, connected: true, status: 'healthy' } : target
      ));
    }
  };

  const disconnectTarget = async (id: string, type: StorageTargetType) => {
    console.log(`Disconnecting ${type} target ${id}`);
    await new Promise(resolve => setTimeout(resolve, 500));
    if (type === 'nvmeof') {
      setNvmeTargets(prev => prev.map(target => 
        target.id === id ? { ...target, connected: false } : target
      ));
    } else {
      setIscsiTargets(prev => prev.map(target => 
        target.id === id ? { ...target, connected: false } : target
      ));
    }
  };

  const deleteTarget = (id: string, type: StorageTargetType) => {
    if (type === 'nvmeof') {
      setNvmeTargets(prev => prev.filter(target => target.id !== id));
    } else {
      setIscsiTargets(prev => prev.filter(target => target.id !== id));
    }
  };

  const handleDiscover = async () => {
    setIsDiscovering(true);
    setDiscoveryError(null);
    setDiscoveredNamespaces([]);
    setSelectedNamespaceId(null); // Force re-selection after discovery
    console.log('Discovering namespaces for target:', formData);
    await new Promise(resolve => setTimeout(resolve, 1500));
    if (formData.address?.includes('192')) {
      setDiscoveredNamespaces(mockDiscoveredNamespaces);
    } else {
      setDiscoveryError('Failed to connect to target. Please check connection details.');
    }
    setIsDiscovering(false);
  };

  const saveTarget = () => {
    if (activeTab === 'nvmeof') {
      if (!selectedNamespaceId) {
        alert("Please discover and select a namespace.");
        return;
      }
      const newTarget: NVMeOFTarget = {
        id: editingTarget || Date.now().toString(),
        name: formData.name || '',
        nqn: formData.nqn || '',
        transport: formData.transport as 'tcp' | 'rdma' | 'fc' || 'tcp',
        address: formData.address || '',
        port: formData.port || '4420',
        nsid: selectedNamespaceId,
        connected: false,
        status: 'healthy'
      };
      if (editingTarget) {
        setNvmeTargets(prev => prev.map(target => 
          target.id === editingTarget ? { ...target, ...newTarget } : target
        ));
      } else {
        setNvmeTargets(prev => [...prev, newTarget]);
      }
    } else {
      const newTarget: iSCSITarget = {
        id: editingTarget || Date.now().toString(),
        name: formData.name || '',
        targetIQN: formData.targetIQN || '',
        portalIP: formData.portalIP || '',
        port: formData.port || '3260',
        lun: formData.lun || 0,
        connected: false,
        status: 'healthy',
        authMethod: formData.authMethod as 'none' | 'chap' || 'none',
        username: formData.username,
        password: formData.password
      };
      if (editingTarget) {
        setIscsiTargets(prev => prev.map(target => 
          target.id === editingTarget ? { ...target, ...newTarget } : target
        ));
      } else {
        setIscsiTargets(prev => [...prev, newTarget]);
      }
    }
    resetForm();
  };

  const resetForm = (soft = false) => {
    if (!soft) setShowAddForm(false);
    setEditingTarget(null);
    setFormData({
      transport: 'tcp',
      port: activeTab === 'nvmeof' ? '4420' : '3260',
      authMethod: 'none'
    });
    // Reset discovery state
    setIsDiscovering(false);
    setDiscoveredNamespaces([]);
    setSelectedNamespaceId(null);
    setDiscoveryError(null);
  };

  const editTarget = (target: NVMeOFTarget | iSCSITarget) => {
    setFormData(target);
    setEditingTarget(target.id);
    if (activeTab === 'nvmeof' && 'nsid' in target) {
        setSelectedNamespaceId(target.nsid);
    }
    setShowAddForm(true);
  };

  const getStatusIcon = (status: string, connected: boolean) => {
    if (!connected) return <WifiOff className="w-4 h-4 text-gray-500" />;
    switch (status) {
      case 'healthy': return <CheckCircle className="w-4 h-4 text-green-500" />;
      case 'degraded': return <AlertTriangle className="w-4 h-4 text-yellow-500" />;
      case 'failed': return <X className="w-4 h-4 text-red-500" />;
      default: return <Wifi className="w-4 h-4 text-blue-500" />;
    }
  };

  const togglePasswordVisibility = (targetId: string) => {
    setShowPasswords(prev => ({ ...prev, [targetId]: !prev[targetId] }));
  };

  const renderNVMeOFTargets = () => (
    <div className="space-y-4">
      {nvmeTargets.map((target) => (
        <div key={target.id} className="bg-white rounded-lg border border-gray-200 p-6">
          <div className="flex items-center justify-between mb-4">
            <div className="flex items-center gap-3">
              {getStatusIcon(target.status, target.connected)}
              <div>
                <h3 className="font-semibold text-gray-900">{target.name}</h3>
                <p className="text-sm text-gray-500">{target.nqn}</p>
              </div>
            </div>
            <div className="flex items-center gap-2">
              <span className={`px-2 py-1 text-xs rounded-full ${target.connected ? 'bg-green-100 text-green-700' : 'bg-gray-100 text-gray-700'}`}>{target.connected ? 'Connected' : 'Disconnected'}</span>
              <button onClick={() => editTarget(target)} className="p-1 hover:bg-gray-100 rounded"><Edit3 className="w-4 h-4 text-gray-500" /></button>
              <button onClick={() => target.connected ? disconnectTarget(target.id, 'nvmeof') : connectTarget(target.id, 'nvmeof')} className={`px-3 py-1 rounded text-sm font-medium ${target.connected ? 'bg-red-100 text-red-700 hover:bg-red-200' : 'bg-blue-100 text-blue-700 hover:bg-blue-200'}`}>{target.connected ? 'Disconnect' : 'Connect'}</button>
              <button onClick={() => deleteTarget(target.id, 'nvmeof')} className="p-1 hover:bg-red-100 rounded text-red-500"><Trash2 className="w-4 h-4" /></button>
            </div>
          </div>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-sm">
            <div><span className="text-gray-500">Transport:</span><span className="ml-2 font-mono">{target.transport.toUpperCase()}</span></div>
            <div><span className="text-gray-500">Address:</span><span className="ml-2 font-mono">{target.address}:{target.port}</span></div>
            <div><span className="text-gray-500">NSID:</span><span className="ml-2 font-bold">{target.nsid}</span></div>
            <div><span className="text-gray-500">Last Connected:</span><span className="ml-2">{target.lastConnected || 'Never'}</span></div>
          </div>
        </div>
      ))}
    </div>
  );

  const renderISCSITargets = () => (
    <div className="space-y-4">
      {iscsiTargets.map((target) => (
        <div key={target.id} className="bg-white rounded-lg border border-gray-200 p-6">
            <div className="flex items-center justify-between mb-4">
                <div className="flex items-center gap-3">{getStatusIcon(target.status, target.connected)}<div><h3 className="font-semibold text-gray-900">{target.name}</h3><p className="text-sm text-gray-500">{target.targetIQN}</p></div></div>
                <div className="flex items-center gap-2">
                    <span className={`px-2 py-1 text-xs rounded-full ${target.connected ? 'bg-green-100 text-green-700' : 'bg-gray-100 text-gray-700'}`}>{target.connected ? 'Connected' : 'Disconnected'}</span>
                    <button onClick={() => editTarget(target)} className="p-1 hover:bg-gray-100 rounded"><Edit3 className="w-4 h-4 text-gray-500" /></button>
                    <button onClick={() => target.connected ? disconnectTarget(target.id, 'iscsi') : connectTarget(target.id, 'iscsi')} className={`px-3 py-1 rounded text-sm font-medium ${target.connected ? 'bg-red-100 text-red-700 hover:bg-red-200' : 'bg-blue-100 text-blue-700 hover:bg-blue-200'}`}>{target.connected ? 'Disconnect' : 'Connect'}</button>
                    <button onClick={() => deleteTarget(target.id, 'iscsi')} className="p-1 hover:bg-red-100 rounded text-red-500"><Trash2 className="w-4 h-4" /></button>
                </div>
            </div>
            <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-sm">
                <div><span className="text-gray-500">Portal:</span><span className="ml-2 font-mono">{target.portalIP}:{target.port}</span></div>
                <div><span className="text-gray-500">LUN:</span><span className="ml-2">{target.lun}</span></div>
                <div><span className="text-gray-500">Capacity:</span><span className="ml-2">{target.capacity || 'Unknown'}</span></div>
                <div><span className="text-gray-500">Auth:</span><span className="ml-2">{target.authMethod?.toUpperCase() || 'NONE'}</span></div>
            </div>
            {target.authMethod === 'chap' && target.username && (<div className="mt-4 p-3 bg-gray-50 rounded"><div className="flex items-center gap-4 text-sm"><div><span className="text-gray-500">Username:</span><span className="ml-2 font-mono">{target.username}</span></div><div className="flex items-center gap-2"><span className="text-gray-500">Password:</span><span className="ml-2 font-mono">{showPasswords[target.id] ? target.password : '••••••••'}</span><button onClick={() => togglePasswordVisibility(target.id)} className="p-1 hover:bg-gray-200 rounded">{showPasswords[target.id] ? <EyeOff className="w-3 h-3 text-gray-400" /> : <Eye className="w-3 h-3 text-gray-400" />}</button></div></div></div>)}
        </div>
      ))}
    </div>
  );

  const renderAddForm = () => (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
      <div className="bg-white rounded-lg p-6 w-full max-w-2xl max-h-[90vh] overflow-y-auto">
        <div className="flex items-center justify-between mb-6">
          <h3 className="text-lg font-semibold">{editingTarget ? 'Edit' : 'Add'} {activeTab === 'nvmeof' ? 'NVMe-oF' : 'iSCSI'} Target</h3>
          <button onClick={() => resetForm()} className="text-gray-500 hover:text-gray-700"><X className="w-5 h-5" /></button>
        </div>
        <form onSubmit={(e) => { e.preventDefault(); saveTarget(); }} className="space-y-4">
            <div><label className="block text-sm font-medium text-gray-700 mb-1">Name</label><input type="text" required value={formData.name || ''} onChange={(e) => handleFormChange('name', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500" placeholder="Storage array name" /></div>
            {activeTab === 'nvmeof' ? (
                <>
                    <div><label className="block text-sm font-medium text-gray-700 mb-1">NQN</label><input type="text" required value={formData.nqn || ''} onChange={(e) => handleFormChange('nqn', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500" placeholder="nqn.2023.io.storage:target1" /></div>
                    <div><label className="block text-sm font-medium text-gray-700 mb-1">Transport</label><select value={formData.transport || 'tcp'} onChange={(e) => handleFormChange('transport', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"><option value="tcp">TCP</option><option value="rdma">RDMA</option><option value="fc">Fibre Channel</option></select></div>
                    <div className="grid grid-cols-2 gap-4">
                        <div><label className="block text-sm font-medium text-gray-700 mb-1">Address</label><input type="text" required value={formData.address || ''} onChange={(e) => handleFormChange('address', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500" placeholder="192.168.1.100"/></div>
                        <div><label className="block text-sm font-medium text-gray-700 mb-1">Port</label><input type="text" required value={formData.port || '4420'} onChange={(e) => handleFormChange('port', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"/></div>
                    </div>
                    
                    {editingTarget && formData.nsid && !discoveredNamespaces.length && !discoveryError && (
                        <div className="my-4 p-3 bg-gray-100 rounded-md border">
                            <label className="block text-sm font-medium text-gray-700">Currently Selected Namespace ID</label>
                            <p className="text-lg font-mono font-bold text-blue-600">{formData.nsid}</p>
                            <p className="text-xs text-gray-500">You can re-run discovery to select a different namespace.</p>
                        </div>
                    )}

                    <div className="flex justify-end pt-2">
                        <button type="button" onClick={handleDiscover} disabled={isDiscovering} className="inline-flex items-center px-4 py-2 text-sm font-medium text-white bg-blue-600 border border-transparent rounded-md shadow-sm hover:bg-blue-700 disabled:bg-blue-300">{isDiscovering ? <><Loader2 className="mr-2 h-4 w-4 animate-spin" />Discovering...</> : <><Search className="mr-2 h-4 w-4" />{editingTarget ? 'Re-Discover Namespaces' : 'Discover Namespaces'}</>}</button>
                    </div>

                    {discoveryError && <div className="text-center py-2 text-red-600 bg-red-50 rounded-lg text-sm"><p>{discoveryError}</p></div>}
                    
                    {discoveredNamespaces.length > 0 && (
                        <div className="mt-4"><h4 className="text-md font-semibold mb-2">Select a Namespace</h4><div className="border rounded-lg overflow-hidden"><table className="min-w-full divide-y divide-gray-200">
                            <thead className="bg-gray-50"><tr><th className="px-3 py-2 text-left text-xs font-medium text-gray-500">Select</th><th className="px-3 py-2 text-left text-xs font-medium text-gray-500">NSID</th><th className="px-3 py-2 text-left text-xs font-medium text-gray-500">Size</th><th className="px-3 py-2 text-left text-xs font-medium text-gray-500">Attached</th></tr></thead>
                            <tbody className="bg-white divide-y divide-gray-200">{discoveredNamespaces.map(ns => (<tr key={ns.nsid} onClick={() => setSelectedNamespaceId(ns.nsid.toString())} className={`cursor-pointer ${selectedNamespaceId === ns.nsid.toString() ? 'bg-blue-50' : 'hover:bg-gray-50'}`}><td className="px-3 py-2"><input type="radio" name="selected-namespace" checked={selectedNamespaceId === ns.nsid.toString()} readOnly className="form-radio h-4 w-4 text-blue-600"/></td><td className="px-3 py-2 font-mono text-sm">{ns.nsid}</td><td className="px-3 py-2 text-sm">{ns.size}</td><td className="px-3 py-2 text-sm">{ns.attached ? 'Yes' : 'No'}</td></tr>))}</tbody>
                        </table></div></div>
                    )}
                </>
            ) : ( // iSCSI Form
                <>
                    <div><label className="block text-sm font-medium text-gray-700 mb-1">Target IQN</label><input type="text" required value={formData.targetIQN || ''} onChange={(e) => handleFormChange('targetIQN', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500" placeholder="iqn.2023.io.storage:target1"/></div>
                    <div className="grid grid-cols-3 gap-4">
                        <div><label className="block text-sm font-medium text-gray-700 mb-1">Portal IP</label><input type="text" required value={formData.portalIP || ''} onChange={(e) => handleFormChange('portalIP', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500" placeholder="192.168.1.100"/></div>
                        <div><label className="block text-sm font-medium text-gray-700 mb-1">Port</label><input type="text" required value={formData.port || '3260'} onChange={(e) => handleFormChange('port', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"/></div>
                        <div><label className="block text-sm font-medium text-gray-700 mb-1">LUN</label><input type="number" min="0" required value={formData.lun === undefined ? '' : formData.lun} onChange={(e) => handleFormChange('lun', parseInt(e.target.value))} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"/></div>
                    </div>
                    <div><label className="block text-sm font-medium text-gray-700 mb-1">Authentication</label><select value={formData.authMethod || 'none'} onChange={(e) => handleFormChange('authMethod', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"><option value="none">None</option><option value="chap">CHAP</option></select></div>
                    {formData.authMethod === 'chap' && (
                        <div className="grid grid-cols-2 gap-4">
                            <div><label className="block text-sm font-medium text-gray-700 mb-1">Username</label><input type="text" required={formData.authMethod === 'chap'} value={formData.username || ''} onChange={(e) => handleFormChange('username', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"/></div>
                            <div><label className="block text-sm font-medium text-gray-700 mb-1">Password</label><input type="password" required={formData.authMethod === 'chap'} value={formData.password || ''} onChange={(e) => handleFormChange('password', e.target.value)} className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"/></div>
                        </div>
                    )}
                </>
            )}
            <div className="flex justify-end gap-3 pt-4 border-t"><button type="button" onClick={() => resetForm()} className="px-4 py-2 text-gray-700 border border-gray-300 rounded-md hover:bg-gray-50">Cancel</button><button type="submit" disabled={activeTab === 'nvmeof' && !selectedNamespaceId} className="px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700 disabled:bg-blue-300"><Save className="w-4 h-4 inline mr-2" />{editingTarget ? 'Update' : 'Add'} Target</button></div>
        </form>
      </div>
    </div>
  );

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between"><div><h2 className="text-2xl font-bold text-gray-900">Remote Storage</h2><p className="text-gray-600">Configure and manage NVMe-oF and iSCSI targets</p></div><div className="flex items-center gap-3"><button onClick={() => { setShowAddForm(true); }} className="flex items-center gap-2 px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700"><Plus className="w-4 h-4" />Add Target</button><button className="flex items-center gap-2 px-4 py-2 border border-gray-300 rounded-md hover:bg-gray-50"><RefreshCw className="w-4 h-4" />Refresh</button></div></div>
      <div className="border-b border-gray-200"><nav className="-mb-px flex space-x-8"><button onClick={() => setActiveTab('nvmeof')} className={`flex items-center gap-2 py-4 px-1 border-b-2 font-medium text-sm ${activeTab === 'nvmeof' ? 'border-blue-500 text-blue-600' : 'border-transparent text-gray-500 hover:text-gray-700 hover:border-gray-300'}`}><Network className="w-5 h-5" />NVMe-oF Targets ({nvmeTargets.length})</button><button onClick={() => setActiveTab('iscsi')} className={`flex items-center gap-2 py-4 px-1 border-b-2 font-medium text-sm ${activeTab === 'iscsi' ? 'border-blue-500 text-blue-600' : 'border-transparent text-gray-500 hover:text-gray-700 hover:border-gray-300'}`}><HardDrive className="w-5 h-5" />iSCSI Targets ({iscsiTargets.length})</button></nav></div>
      <div className="bg-gray-50 rounded-lg p-6">
        {activeTab === 'nvmeof' ? renderNVMeOFTargets() : renderISCSITargets()}
        {((activeTab === 'nvmeof' && nvmeTargets.length === 0) || (activeTab === 'iscsi' && iscsiTargets.length === 0)) && (<div className="text-center py-12"><HardDrive className="w-12 h-12 text-gray-400 mx-auto mb-4" /><h3 className="text-lg font-medium text-gray-900 mb-2">No {activeTab === 'nvmeof' ? 'NVMe-oF' : 'iSCSI'} targets configured</h3><p className="text-gray-600 mb-4">Add your first {activeTab === 'nvmeof' ? 'NVMe-oF' : 'iSCSI'} target to get started</p><button onClick={() => setShowAddForm(true)} className="inline-flex items-center gap-2 px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700"><Plus className="w-4 h-4" />Add {activeTab === 'nvmeof' ? 'NVMe-oF' : 'iSCSI'} Target</button></div>)}
      </div>
      {showAddForm && renderAddForm()}
    </div>
  );
};

export default RemoteStorageTab;
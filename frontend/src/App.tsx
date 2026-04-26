import { Routes, Route, NavLink, Navigate } from 'react-router-dom';
import Dashboard from './pages/Dashboard';
import Velocity from './pages/Velocity';
import VelocityEndpoint from './pages/VelocityEndpoint';
import VelocityCompare from './pages/VelocityCompare';
import VelocityTrace from './pages/VelocityTrace';
import Evaluation from './pages/Evaluation';
import EvalRunDetail from './pages/EvalRunDetail';
import VelocityTest from './pages/VelocityTest';
import VelocityImprovement from './pages/VelocityImprovement';
import RunnerMonitor from './pages/RunnerMonitor';
import Fleet from './pages/Fleet';
import {
  UIBridgeProvider,
  AutoRegisterProvider,
  CommandRelayListener,
} from '@qontinui/ui-bridge/react';
import { BootIdWatcher } from './components/BootIdWatcher';
import { BuildRefreshBanner } from './components/BuildRefreshBanner';

export default function App() {
  return (
    <UIBridgeProvider features={{ control: true }}>
      <AutoRegisterProvider>
        <CommandRelayListener
          basePath="/supervisor-bridge"
          appId="qontinui-supervisor-dashboard"
          appName="Qontinui Supervisor"
          appType="dashboard"
        />
        <BootIdWatcher />
        <BuildRefreshBanner />
        <div className="layout">
          <aside className="sidebar">
            <div className="sidebar-logo">Supervisor</div>
            <ul className="sidebar-nav">
              <li>
                <NavLink to="/dashboard" className={({ isActive }) => (isActive ? 'active' : '')}>
                  Dashboard
                </NavLink>
              </li>
              <li>
                <NavLink to="/velocity" className={({ isActive }) => (isActive ? 'active' : '')}>
                  Velocity
                </NavLink>
              </li>
              <li>
                <NavLink
                  to="/velocity/compare"
                  className={({ isActive }) => (isActive ? 'active' : '')}
                >
                  Compare
                </NavLink>
              </li>
              <li>
                <NavLink
                  to="/velocity/trace"
                  className={({ isActive }) => (isActive ? 'active' : '')}
                >
                  Trace
                </NavLink>
              </li>
              <li>
                <NavLink
                  to="/runner-monitor"
                  className={({ isActive }) => (isActive ? 'active' : '')}
                >
                  Runner Monitor
                </NavLink>
              </li>
              <li>
                <NavLink to="/fleet" className={({ isActive }) => (isActive ? 'active' : '')}>
                  Fleet
                </NavLink>
              </li>
              <li>
                <NavLink to="/evaluation" className={({ isActive }) => (isActive ? 'active' : '')}>
                  Evaluation
                </NavLink>
              </li>
              <li>
                <NavLink
                  to="/velocity-tests"
                  className={({ isActive }) => (isActive ? 'active' : '')}
                >
                  Velocity Tests
                </NavLink>
              </li>
              <li>
                <NavLink
                  to="/velocity-improvement"
                  className={({ isActive }) => (isActive ? 'active' : '')}
                >
                  Improvement
                </NavLink>
              </li>
            </ul>
          </aside>
          <main className="main-content">
            <Routes>
              <Route path="/" element={<Navigate to="/dashboard" replace />} />
              <Route path="/dashboard" element={<Dashboard />} />
              <Route path="/velocity" element={<Velocity />} />
              <Route path="/velocity/endpoint" element={<VelocityEndpoint />} />
              <Route path="/velocity/compare" element={<VelocityCompare />} />
              <Route path="/velocity/trace" element={<VelocityTrace />} />
              <Route path="/runner-monitor" element={<RunnerMonitor />} />
              <Route path="/fleet" element={<Fleet />} />
              <Route path="/evaluation" element={<Evaluation />} />
              <Route path="/evaluation/run/:id" element={<EvalRunDetail />} />
              <Route path="/velocity-tests" element={<VelocityTest />} />
              <Route path="/velocity-improvement" element={<VelocityImprovement />} />
            </Routes>
          </main>
        </div>
      </AutoRegisterProvider>
    </UIBridgeProvider>
  );
}

import { Routes, Route, NavLink, Navigate } from 'react-router-dom';
import Dashboard from './pages/Dashboard';
import Velocity from './pages/Velocity';
import VelocityEndpoint from './pages/VelocityEndpoint';
import VelocityCompare from './pages/VelocityCompare';
import VelocityTrace from './pages/VelocityTrace';
import Evaluation from './pages/Evaluation';
import EvalRunDetail from './pages/EvalRunDetail';

export default function App() {
  return (
    <div className="layout">
      <aside className="sidebar">
        <div className="sidebar-logo">Supervisor</div>
        <ul className="sidebar-nav">
          <li><NavLink to="/dashboard" className={({ isActive }) => isActive ? 'active' : ''}>Dashboard</NavLink></li>
          <li><NavLink to="/velocity" className={({ isActive }) => isActive ? 'active' : ''}>Velocity</NavLink></li>
          <li><NavLink to="/velocity/compare" className={({ isActive }) => isActive ? 'active' : ''}>Compare</NavLink></li>
          <li><NavLink to="/velocity/trace" className={({ isActive }) => isActive ? 'active' : ''}>Trace</NavLink></li>
          <li><NavLink to="/evaluation" className={({ isActive }) => isActive ? 'active' : ''}>Evaluation</NavLink></li>
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
          <Route path="/evaluation" element={<Evaluation />} />
          <Route path="/evaluation/run/:id" element={<EvalRunDetail />} />
        </Routes>
      </main>
    </div>
  );
}
